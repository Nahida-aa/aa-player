#!/usr/bin/env bun
// 从 logo.svg 生成各尺寸 PNG 应用图标，输出到 freedesktop hicolor 目录布局：
//   resources/icons/hicolor/<size>x<size>/apps/aa-player.png
// 产物随仓库提交，AUR PKGBUILD / just install 直接拷贝，无需再装 node/bun。
//
// 用法：cd scripts && bun install && bun gen-icons.mjs
import { Resvg } from "@resvg/resvg-js";
import { dirname, join } from "node:path";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";

const root = new URL("..", import.meta.url).pathname;
const svgPath = join(root, "packages/assets/assets/images/logo.svg");
const outRoot = join(root, "resources/icons/hicolor");

// hicolor 常用尺寸；小尺寸保证任务栏/Alt-Tab 下不糊。
const sizes = [512, 256, 128, 64, 48, 32];

const svg = readFileSync(svgPath, "utf8");
for (const size of sizes) {
  const png = new Resvg(svg, {
    fitTo: { mode: "width", value: size },
    // logo.svg 自带深色圆角背景与透明边距，按 viewBox 整图缩放即可。
  })
    .render()
    .asPng();
  const out = join(outRoot, `${size}x${size}`, "apps", "aa-player.png");
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, png);
  console.log(`✓ ${out} (${png.length} bytes)`);
}
