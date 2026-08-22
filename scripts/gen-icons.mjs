#!/usr/bin/env bun
// 从 logo.svg 生成各尺寸 PNG 应用图标，输出到 freedesktop hicolor 目录布局：
//   resources/icons/hicolor/<size>x<size>/apps/aa-player.png
// 另打包多尺寸 ICO（PNG 压缩条目，Vista+ 支持）供 Windows 资源嵌入：
//   resources/icons/aa-player.ico
// 产物随仓库提交，AUR PKGBUILD / just install / Windows 构建直接拷贝，
// 无需再装 node/bun。
//
// 用法：cd scripts && bun install && bun gen-icons.mjs
import { Resvg } from "@resvg/resvg-js";
import { dirname, join } from "node:path";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";

const root = new URL("..", import.meta.url).pathname;
const svgPath = join(root, "packages/assets/assets/images/logo.svg");
const outRoot = join(root, "resources/icons/hicolor");
const icoPath = join(root, "resources/icons/aa-player.ico");

// hicolor 常用尺寸；小尺寸保证任务栏/Alt-Tab 下不糊。
const sizes = [512, 256, 128, 64, 48, 32];
// Windows 资源浏览器/任务栏需要的尺寸（比 hicolor 多一个 16）。
const icoSizes = [256, 128, 64, 48, 32, 16];

const svg = readFileSync(svgPath, "utf8");

function renderPng(size) {
  return new Resvg(svg, {
    fitTo: { mode: "width", value: size },
    // logo.svg 自带深色圆角背景与透明边距，按 viewBox 整图缩放即可。
  })
    .render()
    .asPng();
}

for (const size of sizes) {
  const png = renderPng(size);
  const out = join(outRoot, `${size}x${size}`, "apps", "aa-player.png");
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, png);
  console.log(`✓ ${out} (${png.length} bytes)`);
}

// ---- 打包 ICO：ICONDIR + ICONDIRENTRY×N + 若干原始 PNG ----
// 手写二进制而不是引入依赖：格式就 6+16N 字节头，PNG 条目原样内嵌。
const pngs = icoSizes.map((s) => ({ size: s, data: renderPng(s) }));
const header = Buffer.alloc(6);
header.writeUInt16LE(0, 0); // reserved
header.writeUInt16LE(1, 2); // type: icon
header.writeUInt16LE(pngs.length, 4);

const entries = [];
let offset = 6 + 16 * pngs.length;
for (const { size, data } of pngs) {
  const e = Buffer.alloc(16);
  e.writeUInt8(size === 256 ? 0 : size, 0); // 宽（256 编码为 0）
  e.writeUInt8(size === 256 ? 0 : size, 1); // 高
  e.writeUInt8(0, 2); // 调色板色数（PNG 条目不用）
  e.writeUInt8(0, 3); // reserved
  e.writeUInt16LE(1, 4); // color planes
  e.writeUInt16LE(32, 6); // bits per pixel
  e.writeUInt32LE(data.length, 8);
  e.writeUInt32LE(offset, 12);
  offset += data.length;
  entries.push(e);
}
const ico = Buffer.concat([header, ...entries, ...pngs.map((p) => p.data)]);
writeFileSync(icoPath, ico);
console.log(`✓ ${icoPath} (${ico.length} bytes, ${icoSizes.join("/")})`);
