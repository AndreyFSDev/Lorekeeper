"""Дообучение LoRA для моделей картинок через kohya sd-scripts.

Запускается приложением: python train_image_lora.py --config <путь к json>.
kohya идёт отдельным процессом, его вывод построчно уходит в лог приложения,
а прогресс печатается маркерами `@@LK@@ {json}` — их разбирает Rust.

Датасет задаётся файлом-конфигом: у sd-scripts `resolution` лежит на уровне
`[[datasets]]`, а не подмножества (иначе «resolution is required»), и шаг
бакетов разный — 64 у SD 1.5, 32 у SDXL.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

MARKER = "@@LK@@"
IMAGE_EXTENSIONS = (".png", ".jpg", ".jpeg", ".webp", ".bmp")

# kohya entry point and bucket grid step per model family.
TRAIN_SCRIPT = {"sd15": "train_network.py", "sdxl": "sdxl_train_network.py"}
BUCKET_STEPS = {"sd15": 64, "sdxl": 32}

# tqdm progress line: "steps:  45%|####  | 9/20 [00:26<00:31,  2.86s/it, avr_loss=0.15]".
# Caching bars have no caption — their absence is the dataset-stage marker.
BAR_RE = re.compile(
    r"^(?:(?P<desc>[^|]*?):\s*)?(?P<percent>\d+)%\|[^|]*\|\s*(?P<step>\d+)/(?P<total>\d+)\s*\[(?P<clock>[^\]]*)\]"
)


def emit(**payload):
    # ensure_ascii: the marker must survive any console code page.
    print(MARKER + " " + json.dumps(payload), flush=True)


def fail(message):
    emit(error=str(message))
    print("ОШИБКА: " + str(message), file=sys.stderr, flush=True)
    sys.exit(1)


def read_config(path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001 - the message goes to the interface
        fail(f"не удалось прочитать конфиг {path}: {exc}")


def count_pairs(image_dir):
    """Пары «картинка + подпись»: kohya берёт подпись из файла рядом с картинкой."""
    images = [p for p in Path(image_dir).iterdir() if p.suffix.lower() in IMAGE_EXTENSIONS]
    captions = sum(1 for p in images if p.with_suffix(".txt").exists())
    return len(images), captions


def toml_string(value):
    """Путь строкой TOML: JSON-строка — та же basic-строка с экранированием.
    Литеральная ('...') ломалась бы о кавычку в пути (C:\\Users\\O'Brien\\...)."""
    return json.dumps(str(value))


def dataset_toml(config):
    family = config.get("family", "sdxl")
    return (
        "[general]\n"
        "enable_bucket = true\n"
        "bucket_no_upscale = true\n"
        "\n"
        "[[datasets]]\n"
        f"resolution = {int(config['resolution'])}\n"
        "batch_size = 1\n"
        "enable_bucket = true\n"
        f"bucket_reso_steps = {BUCKET_STEPS.get(family, 32)}\n"
        "  [[datasets.subsets]]\n"
        f"  image_dir = {toml_string(config['dataset_dir'])}\n"
        "  caption_extension = '.txt'\n"
        f"  num_repeats = {int(config['repeats'])}\n"
    )


def build_command(config, sd_scripts_dir, dataset_config):
    """Команда повторяет живой прогон спайка: только UNet, dim 8, кэш латентов
    и выходов текстового энкодера, AdamW8bit, bf16.

    --save_every_n_steps не передаётся: при значении, равном max_train_steps,
    kohya писал шаговый дубль <имя>-stepNNNNNNNN рядом с финальным LoRA."""
    script = TRAIN_SCRIPT.get(config.get("family", "sdxl"), "sdxl_train_network.py")
    command = [
        sys.executable,
        str(sd_scripts_dir / script),
        "--dataset_config",
        str(dataset_config),
        "--pretrained_model_name_or_path",
        config["base"],
        "--output_dir",
        config["output_dir"],
        "--output_name",
        config["output_name"],
        "--network_module",
        "networks.lora",
        "--network_dim",
        str(int(config["network_dim"])),
        "--network_alpha",
        str(int(config["network_alpha"])),
        "--max_train_steps",
        str(int(config["max_train_steps"])),
        "--save_model_as",
        "safetensors",
        "--mixed_precision",
        "bf16",
        "--gradient_checkpointing",
        "--cache_latents",
        "--network_train_unet_only",
        "--no_half_vae",
        "--console_log_simple",
        "--learning_rate",
        str(config["learning_rate"]),
        "--optimizer_type",
        "AdamW8bit",
        "--seed",
        str(int(config["seed"])),
    ]
    # The flag is declared only in the SDXL parser (sdxl_train_util.add_sdxl_training_arguments);
    # train_network.py for SD 1.5 parses arguments strictly and exited with code 2.
    if script == "sdxl_train_network.py":
        command.append("--cache_text_encoder_outputs")
    return command


def parse_bar(text):
    match = BAR_RE.match(text)
    if not match:
        return None
    clock = match.group("clock")
    speed = re.search(r"([\d.]+)(s/it|it/s)", clock)
    loss = re.search(r"avr_loss=([\d.]+)", clock)
    left = re.search(r"<(\d+):(\d+)", clock)
    return {
        "desc": (match.group("desc") or "").strip(),
        "step": int(match.group("step")),
        "total": int(match.group("total")),
        "loss": float(loss.group(1)) if loss else None,
        "speed": float(speed.group(1)) if speed else None,
        "eta": int(left.group(1)) * 60 + int(left.group(2)) if left else None,
    }


class Progress:
    """Разбор вывода kohya: полосы прогресса — в маркеры, остальное — в лог."""

    def __init__(self, learning_rate):
        self.learning_rate = learning_rate
        # Before the first "steps" bar kohya caches latents and encoder outputs.
        self.stage = "dataset"
        self.cache_reported = False
        self.last_step = -1

    def line(self, text):
        text = text.strip()
        if not text:
            return
        bar = parse_bar(text)
        if bar is None:
            print(text, flush=True)
            return
        if bar["desc"] == "steps":
            self.stage = "steps"
        if self.stage != "steps":
            # There are many caching bars and they are fast: only the stage
            # change goes to the UI, otherwise cache step numbers would be confused with training steps.
            if not self.cache_reported:
                self.cache_reported = True
                emit(stage="dataset")
            return
        # tqdm redraws the same bar several times per step.
        if bar["step"] == self.last_step:
            return
        self.last_step = bar["step"]
        emit(
            stage="steps",
            step=bar["step"],
            total=bar["total"],
            loss=bar["loss"],
            # Without --lr_scheduler kohya uses a constant learning rate.
            lr=self.learning_rate,
            speed=bar["speed"],
            eta=bar["eta"],
        )


def run_kohya(command, env, cwd, learning_rate):
    started = time.time()
    progress = Progress(learning_rate)
    process = subprocess.Popen(
        command,
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=0,
    )
    tail = []
    pending = b""
    while True:
        chunk = os.read(process.stdout.fileno(), 8192)
        if not chunk:
            break
        pending += chunk
        # tqdm redraws the line via \r: without parsing on it, progress
        # only becomes visible at the end of the run.
        while True:
            cut = re.search(rb"[\r\n]", pending)
            if not cut:
                break
            raw, pending = pending[: cut.start()], pending[cut.end() :]
            text = raw.decode("utf-8", "replace")
            progress.line(text)
            tail.append(text)
            del tail[:-8]
    process.wait()
    return process.returncode, tail, time.time() - started


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    args = parser.parse_args()
    config = read_config(args.config)

    for key, value in (config.get("env") or {}).items():
        os.environ[str(key)] = str(value)
    os.environ.setdefault("PYTHONIOENCODING", "utf-8")
    os.environ.setdefault("HF_HUB_DISABLE_SYMLINKS_WARNING", "1")
    # Redirect our own output to UTF-8 too: kohya lines go out as is.
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    emit(stage="loading", base=Path(config["base"]).name, family=config.get("family"))

    image_dir = Path(config["dataset_dir"])
    if not image_dir.is_dir():
        fail(f"папка датасета не найдена: {image_dir}")
    images, captions = count_pairs(image_dir)
    if images == 0:
        fail(f"в папке датасета нет картинок: {image_dir}")
    if captions < images:
        print(f"ВНИМАНИЕ: у {images - captions} картинок нет .txt — они пойдут с пустой подписью", flush=True)

    sd_scripts_dir = Path(config["sd_scripts_dir"])
    run_dir = Path(config["run_dir"])
    run_dir.mkdir(parents=True, exist_ok=True)
    dataset_config = run_dir / "dataset.toml"
    dataset_config.write_text(dataset_toml(config), encoding="utf-8")
    output_dir = Path(config["output_dir"])
    output_dir.mkdir(parents=True, exist_ok=True)

    command = build_command(config, sd_scripts_dir, dataset_config)
    print("КОМАНДА: " + " ".join(command), flush=True)
    print(dataset_config.read_text(encoding="utf-8"), flush=True)
    emit(stage="dataset", images=images, captions=captions, total=int(config["max_train_steps"]))

    code, tail, seconds = run_kohya(command, os.environ.copy(), sd_scripts_dir, config["learning_rate"])
    if code != 0:
        fail("kohya завершился с кодом {}: {}".format(code, " | ".join(tail[-4:])))

    lora = output_dir / (config["output_name"] + ".safetensors")
    if not lora.exists():
        fail(f"обучение прошло, но {lora} не появился")
    emit(stage="saving", path=str(lora))
    emit(
        stage="done",
        lora=str(lora),
        seconds=round(seconds, 1),
        size_mb=round(lora.stat().st_size / 1048576, 1),
    )


if __name__ == "__main__":
    main()
