// Мелкие кадры .ico — из упрощённой SVG (в 16px детали превращаются в грязь),
// крупные — из детальной. Плюс PNG-размеры, которые просит бандлер Tauri.
const sharp = require('sharp');
const pngToIco = require('png-to-ico').default || require('png-to-ico');
const fs = require('fs');

const dir = 'src-tauri/icons';
const source512 = `${dir}/512x512.png`;

const pngFrom512 = (size) =>
  sharp(source512).resize(size, size, { kernel: sharp.kernel.lanczos3 }).png().toBuffer();

const icoFrom512 = async () => {
  const frames = await Promise.all([16, 24, 32, 48, 64, 128, 256].map(pngFrom512));
  return pngToIco(frames);
};

(async () => {
  // Иконка exe, ярлыка, трея и логотип в интерфейсе — рубиновое сердце без
  // подложки (растровый исходник владельца, 1024 px).
  fs.writeFileSync(
    source512,
    await sharp(`${dir}/ruby-heart.png`)
      .resize(512, 512, { kernel: sharp.kernel.lanczos3 })
      .png()
      .toBuffer(),
  );

  // Строим icon.ico и все PNG-размеры напрямую из 512x512.png
  fs.writeFileSync(`${dir}/icon.ico`, await icoFrom512());
  fs.writeFileSync(`${dir}/32x32.png`, await pngFrom512(32));
  fs.writeFileSync(`${dir}/128x128.png`, await pngFrom512(128));
  fs.writeFileSync(`${dir}/128x128@2x.png`, await pngFrom512(256));
  console.log('иконки собраны из 512x512.png');
})().catch((e) => {
  console.error('ERR', e.message);
  process.exit(1);
});
