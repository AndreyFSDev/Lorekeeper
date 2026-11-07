"""Дообучение LoRA для текстовой модели (QLoRA 4-bit) под Ollama.

Запускается приложением: python train_text_lora.py --config <путь к json>.
Прогресс печатается строками-маркерами `@@LK@@ {json}` — их разбирает Rust
и показывает в интерфейсе; всё остальное уходит в лог-файл.

Намеренно не используется trl: API SFTTrainer меняется между версиями, а
прямой transformers.Trainer + peft даёт тот же результат с меньшим числом
зависимостей и полным контролем над маскированием промпта.
"""

import argparse
import json
import math
import os
import sys
import time
from pathlib import Path

MARKER = "@@LK@@"


def _finite(value):
    """Не-конечные float → None: NaN/inf не сериализуются в JSON, и маркер с
    ними (A3-03) отбрасывается на стороне Rust целиком — интерфейс замирает."""
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    if isinstance(value, dict):
        return {key: _finite(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_finite(item) for item in value]
    return value


def emit(**payload):
    # ensure_ascii: markers must survive any console code page —
    # Cyrillic in statuses otherwise turns to garbage at the intermediate layer.
    # allow_nan=False is a safeguard: the marker must be strict JSON.
    print(MARKER + " " + json.dumps(_finite(payload), allow_nan=False), flush=True)


def fail(message):
    emit(error=str(message))
    print("ОШИБКА: " + str(message), file=sys.stderr, flush=True)
    sys.exit(1)


def read_config(path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001 - the message goes to the UI
        fail(f"не удалось прочитать конфиг {path}: {exc}")


def iter_records(dataset_path):
    """Записи датасета без мета-строки `__lk_meta` (A3-05): мета несёт режим и
    не является примером."""
    with open(dataset_path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            if isinstance(record, dict) and "__lk_meta" in record:
                continue
            yield record


def build_chat_dataset(dataset_path, tokenizer, max_seq_length):
    from datasets import Dataset

    records = []
    for index, record in enumerate(iter_records(dataset_path), start=1):
        if not isinstance(record, dict) or not isinstance(record.get("messages"), list):
            # Fail closed (A3-05): a mixed dataset used to silently drop
            # non-chat records in filter.
            raise ValueError(f"запись {index} не в режиме chat: нет messages")
        records.append(record)

    def encode(record):
        messages = record.get("messages") or []
        # Label one full render by character offsets rather than by prefix
        # difference: the Qwen3 template draws an empty <think> only for the last
        # answer, and the prefix messages[:i+1] does not continue into messages[:i+2] —
        # the labels of every answer but the last shifted by a few tokens.
        text = tokenizer.apply_chat_template(
            messages,
            tokenize=False,
            add_generation_prompt=False,
            enable_thinking=False,
        )
        eos = tokenizer.eos_token or ""
        spans = []
        cursor = 0
        for message in messages:
            content = message.get("content") or ""
            start = text.find(content, cursor) if content else -1
            if start < 0:
                continue
            end = start + len(content)
            if message.get("role") == "assistant":
                # Teach the turn end too, otherwise the model never learns to stop.
                if eos and text.startswith(eos, end):
                    end += len(eos)
                spans.append((start, end))
            cursor = end
        encoded = tokenizer(text, add_special_tokens=False, return_offsets_mapping=True)
        input_ids = encoded["input_ids"]
        labels = [
            token if any(begin < span_end and finish > span_start for span_start, span_end in spans) else -100
            for token, (begin, finish) in zip(input_ids, encoded["offset_mapping"])
        ]
        input_ids = input_ids[:max_seq_length]
        labels = labels[:max_seq_length]
        return {"input_ids": input_ids, "labels": labels, "length": len(input_ids)}

    dataset = Dataset.from_list(records).map(encode, remove_columns=["messages"])
    # Empty examples (no assistant answers) only spoil batches.
    return dataset.filter(lambda row: any(label != -100 for label in row["labels"]))


def build_text_dataset(dataset_path, tokenizer, max_seq_length):
    from datasets import Dataset

    chunks = []
    for index, record in enumerate(iter_records(dataset_path), start=1):
        text = record.get("text", "") if isinstance(record, dict) else ""
        if not text:
            # Fail closed (A3-05): a record of another kind is an error, not a skip.
            raise ValueError(f"запись {index} не в режиме text: нет text")
        ids = tokenizer(text, add_special_tokens=False)["input_ids"]
        for start in range(0, len(ids), max_seq_length):
            chunk = ids[start : start + max_seq_length]
            # Fragments shorter than 64 tokens give no useful signal.
            if len(chunk) >= 64:
                chunks.append({"input_ids": chunk, "labels": list(chunk), "length": len(chunk)})
    return Dataset.from_list(chunks)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    args = parser.parse_args()
    config = read_config(args.config)

    for key, value in (config.get("env") or {}).items():
        os.environ[str(key)] = str(value)
    os.environ.setdefault("HF_HUB_DISABLE_SYMLINKS_WARNING", "1")

    # Corporate/antivirus proxies replace the TLS chain: certifi from the
    # venv does not know such certificates and download from Hugging Face fails with
    # CERTIFICATE_VERIFY_FAILED. truststore connects the Windows store.
    try:
        import truststore

        truststore.inject_into_ssl()
    except Exception:  # noqa: BLE001 - without truststore the normal path works
        pass

    output_dir = Path(config["output_dir"])
    output_dir.mkdir(parents=True, exist_ok=True)

    try:
        import torch
        from peft import LoraConfig, get_peft_model, prepare_model_for_kbit_training
        from transformers import (
            AutoModelForCausalLM,
            AutoTokenizer,
            BitsAndBytesConfig,
            DataCollatorForSeq2Seq,
            Trainer,
            TrainerCallback,
            TrainingArguments,
        )
    except Exception as exc:  # noqa: BLE001
        fail(f"не удалось импортировать стек обучения: {exc}")

    if not torch.cuda.is_available():
        fail("CUDA недоступна: обучение требует видеокарту NVIDIA")

    emit(stage="loading", cuda=torch.cuda.get_device_name(0), torch=torch.__version__)

    base_model = config["base_model"]
    max_seq_length = int(config.get("max_seq_length", 1024))
    mode = config.get("mode")
    if mode not in ("chat", "text"):
        # Fail closed (A3-05): an unknown mode does not turn into chat —
        # such a dataset silently lost records of one of the kinds.
        fail(f"в конфиге не указан режим датасета (chat/text): {mode!r}")

    try:
        tokenizer = AutoTokenizer.from_pretrained(base_model, trust_remote_code=True)
    except Exception as exc:  # noqa: BLE001
        fail(f"не удалось загрузить токенизатор {base_model}: {exc}")
    if tokenizer.pad_token is None:
        tokenizer.pad_token = tokenizer.eos_token

    try:
        dataset = (
            build_text_dataset(config["dataset"], tokenizer, max_seq_length)
            if mode == "text"
            else build_chat_dataset(config["dataset"], tokenizer, max_seq_length)
        )
    except Exception as exc:  # noqa: BLE001
        fail(f"ошибка подготовки датасета: {exc}")

    examples = len(dataset)
    if examples == 0:
        fail("в датасете нет пригодных примеров")
    emit(stage="dataset", examples=examples, mode=mode)

    quantization = BitsAndBytesConfig(
        load_in_4bit=True,
        bnb_4bit_quant_type="nf4",
        bnb_4bit_use_double_quant=True,
        bnb_4bit_compute_dtype=torch.bfloat16,
    )
    try:
        model = AutoModelForCausalLM.from_pretrained(
            base_model,
            quantization_config=quantization,
            torch_dtype=torch.bfloat16,
            device_map={"": 0},
            attn_implementation="sdpa",
            trust_remote_code=True,
        )
    except Exception as exc:  # noqa: BLE001
        fail(f"не удалось загрузить модель {base_model}: {exc}")

    model.config.use_cache = False
    model = prepare_model_for_kbit_training(model, use_gradient_checkpointing=True)

    target_modules = config.get("target_modules") or [
        "q_proj",
        "k_proj",
        "v_proj",
        "o_proj",
        "gate_proj",
        "up_proj",
        "down_proj",
    ]
    lora = LoraConfig(
        r=int(config.get("lora_r", 16)),
        lora_alpha=int(config.get("lora_alpha", 32)),
        lora_dropout=float(config.get("lora_dropout", 0.05)),
        bias="none",
        task_type="CAUSAL_LM",
        target_modules=list(target_modules),
    )
    model = get_peft_model(model, lora)
    trainable = sum(p.numel() for p in model.parameters() if p.requires_grad)
    total = sum(p.numel() for p in model.parameters())
    emit(stage="model", trainable=trainable, total=total)

    train_args = TrainingArguments(
        output_dir=str(output_dir),
        per_device_train_batch_size=int(config.get("batch_size", 1)),
        gradient_accumulation_steps=int(config.get("grad_accum", 8)),
        learning_rate=float(config.get("learning_rate", 2e-4)),
        num_train_epochs=float(config.get("epochs", 2)),
        max_steps=int(config.get("max_steps", -1)),
        lr_scheduler_type=config.get("lr_scheduler", "cosine"),
        warmup_ratio=float(config.get("warmup_ratio", 0.03)),
        optim=config.get("optim", "paged_adamw_8bit"),
        bf16=True,
        gradient_checkpointing=True,
        logging_steps=1,
        save_steps=int(config.get("save_steps", 50)),
        save_total_limit=2,
        report_to=[],
        seed=int(config.get("seed", 42)),
        dataloader_pin_memory=False,
        remove_unused_columns=False,
        gradient_checkpointing_kwargs={"use_reentrant": False},
    )

    started = time.time()

    class ProgressCallback(TrainerCallback):
        # **kwargs is required: different transformers versions pass extra
        # arguments to callbacks (model, processing_class, etc.).
        def on_log(self, _args, state, _control, logs=None, **_kwargs):
            logs = logs or {}
            if "loss" in logs:
                emit(
                    step=int(state.global_step),
                    total=int(state.max_steps),
                    loss=round(float(logs["loss"]), 4),
                    epoch=round(float(state.epoch or 0), 3),
                    lr=logs.get("learning_rate"),
                    elapsed=round(time.time() - started, 1),
                )

        def on_save(self, _args, state, _control, **_kwargs):
            emit(saved_checkpoint=int(state.global_step))

    trainer = Trainer(
        model=model,
        args=train_args,
        train_dataset=dataset,
        data_collator=DataCollatorForSeq2Seq(tokenizer, padding=True, label_pad_token_id=-100),
        callbacks=[ProgressCallback()],
    )

    resume_from = config.get("resume_from_checkpoint")
    try:
        trainer.train(resume_from_checkpoint=resume_from or None)
    except KeyboardInterrupt:
        emit(stage="cancelled")
        raise
    except Exception as exc:  # noqa: BLE001
        fail(f"обучение прервано: {exc}")

    model.save_pretrained(str(output_dir))
    tokenizer.save_pretrained(str(output_dir))
    emit(stage="done", adapter=str(output_dir), seconds=round(time.time() - started, 1))


if __name__ == "__main__":
    main()
