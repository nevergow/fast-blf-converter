#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从单张源图生成三份图标产物（开发期脚本，不进打包链路）。

用途：``gui/assets/icon.png`` 是**唯一的图标源**（512×512 方形图，改稿只换这一张再重跑
本脚本），派生出：

* ``gui/assets/icon.png``  —— 运行期 Qt 用的图标；
* ``packaging/icon.ico``   —— Windows exe 资源图标（PyInstaller ``EXE(icon=...)``）；
* ``packaging/icon.icns``  —— macOS 应用图标（将来做 .app / 手工换 Dock 图标时用）。

为什么要脚本：图标改一次稿，三种格式必须同步；手工用 Preview / 在线站点转换很容易漏
尺寸（ico 缺 256、icns 缺 @2x），Windows / macOS 会各自回退到糊掉的缩放版。

**双尺寸美术**：完整构图是横向的（左侧像素飞散 + 中间箭头 + 右侧速度线），缩到 16–32px
只剩一团噪点。因此 ≤32px 的槽位改用以红色 FBC 块为中心的紧凑裁切（``compact_crop``），
48px 起恢复完整构图 —— 这与系统图标集"小尺寸简化"的常规做法一致。

依赖：Pillow（**只在开发期**用，不是运行期依赖 —— 生成好的 .ico / .icns 随仓库提交，
CI 打包不需要装 Pillow）。

用法::

    .venv/bin/python scripts/make_icons.py                     # 默认源图，写全部产物
    .venv/bin/python scripts/make_icons.py --source new.png    # 换稿
    .venv/bin/python scripts/make_icons.py --preview           # 额外输出尺寸检查图
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SOURCE = ROOT / "gui" / "assets" / "icon.png"

#: 运行期 Qt 图标的边长。
RUNTIME_SIZE = 512
#: 母版边长：icns 的 512@2x 槽位要 1024px。
MASTER_SIZE = 1024

#: 该尺寸及以下使用紧凑裁切美术（大于它用完整构图）。
SMALL_CUTOFF = 32
#: 紧凑裁切的边长 = 红块宽度 × 该系数（1.85 实测：红块+箭头撑满画布，又不切掉箭头尖端）。
COMPACT_SCALE = 1.85

#: Windows ico 要覆盖的全部尺寸：从标题栏/任务栏小图到资源管理器大图标视图。
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)

#: iconutil 要求的 iconset 文件名 → 像素边长。
ICONSET = {
    "icon_16x16.png": 16,
    "icon_16x16@2x.png": 32,
    "icon_32x32.png": 32,
    "icon_32x32@2x.png": 64,
    "icon_128x128.png": 128,
    "icon_128x128@2x.png": 256,
    "icon_256x256.png": 256,
    "icon_256x256@2x.png": 512,
    "icon_512x512.png": 512,
    "icon_512x512@2x.png": 1024,
}


def _pil():
    """开发期依赖探针：Pillow 不随应用分发，缺了直接给出可执行的安装命令。"""
    try:
        from PIL import Image, ImageChops, ImageFilter, ImageOps
    except ImportError:  # pragma: no cover - 只在没装 Pillow 的机器上走到
        raise SystemExit("[ICON] 需要开发期依赖 Pillow：.venv/bin/pip install Pillow")
    return Image, ImageChops, ImageFilter, ImageOps


def load_source(path: Path):
    """读源图并归一化：按 EXIF 转正、转 RGBA、非方形补成方形。"""
    Image, _ImageChops, _ImageFilter, ImageOps = _pil()
    if not path.is_file():
        raise SystemExit(f"[ICON] 源图不存在：{path}")
    img = ImageOps.exif_transpose(Image.open(path)).convert("RGBA")
    width, height = img.size
    if width == height:
        return img
    # 非方形：以长边为边长补正方形，底色取左上角像素（本项目图都是浅灰底 #F9F9F9）。
    side = max(width, height)
    canvas = Image.new("RGBA", (side, side), img.getpixel((0, 0)))
    canvas.paste(img, ((side - width) // 2, (side - height) // 2))
    print(f"[ICON] 源图 {width}x{height} 非方形，已补底到 {side}x{side}")
    return canvas


def _red_bbox(img) -> tuple[int, int, int, int] | None:
    """定位红色 FBC 块的包围盒；找不到红色时返回 None（调用方回退到完整构图）。"""
    Image, ImageChops, _ImageFilter, _ImageOps = _pil()
    r, g, b = img.convert("RGB").split()
    # 红 = R 足够亮，且 R 明显高于 G/B 中的较大者（纯灰/黑白的像素差值为 0）。
    redness = ImageChops.subtract(r, ImageChops.lighter(g, b)).point(lambda v: 255 if v > 60 else 0)
    brightness = r.point(lambda v: 255 if v > 120 else 0)
    return ImageChops.multiply(redness, brightness).getbbox()


def compact_crop(master):
    """以红色 FBC 块为中心裁一块正方形，专供 16–32px 槽位。"""
    box = _red_bbox(master)
    if box is None:
        print("[ICON] 未检测到红色主体，小尺寸沿用完整构图")
        return master
    cx, cy = (box[0] + box[2]) / 2.0, (box[1] + box[3]) / 2.0
    side = max(box[2] - box[0], 1) * COMPACT_SCALE
    left = min(max(cx - side / 2.0, 0), master.width - side)
    top = min(max(cy - side / 2.0, 0), master.height - side)
    crop = master.crop((int(round(left)), int(round(top)),
                        int(round(left + side)), int(round(top + side))))
    print(f"[ICON] 紧凑裁切 {crop.width}x{crop.height} @ ({int(left)},{int(top)}) 供 ≤{SMALL_CUTOFF}px 使用")
    return crop


def art_for(master, compact, size: int):
    """按目标尺寸选美术：小尺寸用紧凑裁切，其余用完整构图。"""
    return compact if size <= SMALL_CUTOFF else master


def resize(master, size: int):
    """从母版重采样到目标尺寸；中尺寸补一点锐化，抵消 LANCZOS 的边缘软化。"""
    Image, _ImageChops, ImageFilter, _ImageOps = _pil()
    if size == master.width:
        return master.copy()
    out = master.resize((size, size), Image.LANCZOS)
    if size >= 64:  # 小于 64px 时锐化只会让边缘发脏
        out = out.filter(ImageFilter.UnsharpMask(radius=1.0, percent=45, threshold=2))
    return out


def write_runtime(source, dest: Path) -> Path:
    """写运行期 PNG。源图已经是 512 时**原样**保存：本脚本默认源就是这份产物，
    若还走一遍 512→1024→512 的重采样，反复运行会一次比一次糊。"""
    dest.parent.mkdir(parents=True, exist_ok=True)
    runtime = source if source.width == RUNTIME_SIZE else resize(source, min(RUNTIME_SIZE, source.width))
    runtime.save(dest, format="PNG", optimize=True)
    print(f"[ICON] 运行期图标 → {dest.relative_to(ROOT)} "
          f"({runtime.width}px, {dest.stat().st_size} B)")
    return dest


def write_ico(master, compact, dest: Path) -> Path:
    dest.parent.mkdir(parents=True, exist_ok=True)
    # 每个尺寸都提供**精确尺寸**的图：Pillow 的 ico 写入器只要 ``append_images`` 里有图，
    # 遇到"没有精确匹配"的尺寸时会拿最后一张去 thumbnail（而不是从母版缩），于是 48px
    # 以上会被填成小图放大不了的残档。因此这里一次性铺满全部尺寸。
    # ``bitmap_format="bmp"``：老式 DIB 条目，兼容性比 PNG 条目更保险（老工具/杀软扫描）。
    exact = [resize(art_for(master, compact, size), size) for size in ICO_SIZES]
    master.save(dest, format="ICO", sizes=[(s, s) for s in ICO_SIZES],
                append_images=exact, bitmap_format="bmp")
    print(f"[ICON] Windows 图标 → {dest.relative_to(ROOT)} "
          f"({len(ICO_SIZES)} 尺寸, {dest.stat().st_size} B)")
    return dest


def write_icns(master, compact, dest: Path) -> Path | None:
    """用 macOS 自带 ``iconutil`` 打 icns；非 mac 开发机跳过（Windows 链路不依赖它）。"""
    iconutil = shutil.which("iconutil")
    if not iconutil:
        print("[ICON] 未找到 iconutil（非 macOS），跳过 .icns 生成")
        return None
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "icon.iconset"
        iconset.mkdir()
        for name, size in ICONSET.items():
            resize(art_for(master, compact, size), size).save(iconset / name, format="PNG")
        dest.parent.mkdir(parents=True, exist_ok=True)
        proc = subprocess.run(
            [iconutil, "--convert", "icns", str(iconset), "--output", str(dest)],
            capture_output=True, text=True,
        )
        if proc.returncode != 0:
            # 不 fail-fast：.icns 只服务 macOS 打包，Windows 产物不依赖它。
            print(f"[ICON] iconutil 失败（rc={proc.returncode}）：{proc.stderr.strip()}")
            return None
    print(f"[ICON] macOS 图标 → {dest.relative_to(ROOT)} "
          f"({len(ICONSET)} 尺寸, {dest.stat().st_size} B)")
    return dest


def write_banner(master, light_dest: Path, dark_dest: Path) -> tuple[Path, Path]:
    """README 顶部横幅（Notion 封面式）：全幅 4:1，浅色/深色两版。

    构图：底色 + 由图案自己放大模糊得到的氛围底 → 图案居中（正片叠底/滤色融入背景，
    所以没有"一块白板"）。浅色版在 GitHub 浅色主题用，深色版给深色主题切。

    两个必须处理干净的点：
    * 正片叠底要求图案底色是**纯白**，源图是 #F9F9F9，直接乘会留一个暗 2% 的矩形；
      所以先从四角泛洪把与边缘连通的近白压成纯白（FBC 字母那种被包住的内部白不动）。
    * 深色版不能反过来简单 invert：黑字变白字的同时砖红会变成青色。这里按饱和度分离，
      只把**中性色**（低饱和）反相，红色原样保留。
    """
    Image, ImageChops, ImageFilter, _ImageOps = _pil()
    from PIL import ImageDraw

    W, H = 1760, 528   # 3.33:1；GitHub 按内容宽约 880 显示 → 正好 2 倍图
    rgb = master.convert("RGB")
    bg = rgb.getpixel((0, 0))
    diff = ImageChops.difference(rgb, Image.new("RGB", rgb.size, bg)).convert("L")
    art = rgb.crop(diff.point(lambda v: 255 if v > 12 else 0).getbbox())
    for xy in ((0, 0), (art.width - 1, 0), (0, art.height - 1), (art.width - 1, art.height - 1)):
        ImageDraw.floodfill(art, xy, (255, 255, 255), thresh=14)

    def cover(img, w, h):
        scale = max(w / img.width, h / img.height)
        big = img.resize((int(img.width * scale) + 1, int(img.height * scale) + 1), Image.LANCZOS)
        left, top = (big.width - w) // 2, (big.height - h) // 2
        return big.crop((left, top, left + w, top + h))

    def vignette():
        """上下两端渐隐回页面底色的蒙版：只作用于**氛围底**，不作用于图案。

        这样横幅上下看不出矩形边，而图案本身不会被淡掉——两者互不影响，图案高度也就不受
        渐隐带宽度限制。
        """
        grad = Image.new("L", (1, H))
        for y in range(H):
            t = abs((y / (H - 1)) * 2 - 1)          # 0 = 中间, 1 = 上下边缘
            f = max(0.0, (t - 0.50) / 0.50)         # 中间 50% 不动，外侧才开始过渡
            grad.putpixel((0, y), int(255 * f ** 1.4))
        return grad.resize((W, H))

    def compose(base_color, art_layer, blend, mode):
        # 氛围底：图案放大模糊后，两端淡成页面底色，再按 blend 混进纯色底
        wash = cover(art_layer, W, H).filter(ImageFilter.GaussianBlur(60))
        wash = Image.composite(Image.new("RGB", (W, H), base_color), wash, vignette())
        base = Image.blend(Image.new("RGB", (W, H), base_color), wash, blend)
        edge = (255, 255, 255) if mode == "multiply" else (0, 0, 0)
        logo = art_layer.resize((int(art_layer.width * (H * 0.70) / art_layer.height),
                                 int(H * 0.70)), Image.LANCZOS)
        layer = Image.new("RGB", (W, H), edge)
        x0, y0 = (W - logo.width) // 2, (H - logo.height) // 2
        layer.paste(logo, (x0, y0))
        merged = ImageChops.multiply(base, layer) if mode == "multiply" else ImageChops.screen(base, layer)
        # 图案左右两端做 6% 宽度的羽化：源图的速度线/粒子本来就在画布边缘被切断，
        # 放到宽横幅里会显得"一刀切"；淡出到背景后看起来是刻意消散。
        feather = int(logo.width * 0.06)
        mask = Image.new("L", (W, H), 255)
        ramp = Image.new("L", (feather, 1))
        for i in range(feather):
            ramp.putpixel((i, 0), int(255 * i / feather))
        mask.paste(ramp.resize((feather, logo.height)), (x0, y0))
        mask.paste(ramp.resize((feather, logo.height)).transpose(Image.FLIP_LEFT_RIGHT),
                   (x0 + logo.width - feather, y0))
        return Image.composite(merged, base, mask)

    # 深色版：中性色反相（黑↔白），彩色（红）保持
    hsv = art.convert("HSV")
    neutral = hsv.split()[1].point(lambda v: 255 if v < 40 else 0)
    dark_art = Image.composite(ImageChops.invert(art), art, neutral)

    # 底色 = 页面底色（GitHub 浅色 #FFFFFF / 深色 #0D1117），横幅才能和页面融为一体
    light = compose((255, 255, 255), art, 0.26, "multiply")
    dark = compose((13, 17, 23), dark_art, 0.30, "screen")

    for img, dest, tag in ((light, light_dest, "浅色"), (dark, dark_dest, "深色")):
        dest.parent.mkdir(parents=True, exist_ok=True)
        img.save(dest, format="PNG", optimize=True)
        print(f"[BANNER] {tag}横幅 → {dest.relative_to(ROOT)} "
              f"({img.width}x{img.height}, {dest.stat().st_size} B)")
    return light_dest, dark_dest


def write_preview(master, compact, dest: Path) -> Path:
    """把常用尺寸并排拼成一张检查图，肉眼确认小尺寸下还认得出。"""
    Image, _ImageChops, _ImageFilter, _ImageOps = _pil()
    sizes = (16, 24, 32, 48, 64, 128)
    gap, pad = 16, 16
    width = sum(sizes) + gap * (len(sizes) + 1)
    height = max(sizes) + pad * 2
    sheet = Image.new("RGBA", (width, height), (255, 255, 255, 255))
    x = gap
    for size in sizes:
        sheet.alpha_composite(resize(art_for(master, compact, size), size),
                              (x, pad + (max(sizes) - size) // 2))
        x += size + gap
    dest.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(dest, format="PNG")
    print(f"[ICON] 尺寸检查图 → {dest}")
    return dest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="由单张源图生成 PNG / ICO / ICNS 图标")
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE,
                        help=f"源图（默认 {DEFAULT_SOURCE.relative_to(ROOT)}）")
    parser.add_argument("--preview", nargs="?", const=str(ROOT / "build" / "icon_preview.png"),
                        default=None, help="额外输出尺寸检查图（可省略路径）")
    args = parser.parse_args(argv)

    Image, _ImageChops, _ImageFilter, _ImageOps = _pil()
    source = args.source.expanduser().resolve()
    origin = load_source(source)
    master = origin.resize((MASTER_SIZE, MASTER_SIZE), Image.LANCZOS)
    compact = compact_crop(master)

    write_runtime(origin, DEFAULT_SOURCE)
    write_ico(master, compact, ROOT / "packaging" / "icon.ico")
    write_icns(master, compact, ROOT / "packaging" / "icon.icns")
    write_banner(master, ROOT / "docs" / "assets" / "banner.png",
                 ROOT / "docs" / "assets" / "banner-dark.png")
    if args.preview:
        write_preview(master, compact, Path(args.preview).expanduser())
    return 0


if __name__ == "__main__":
    sys.exit(main())
