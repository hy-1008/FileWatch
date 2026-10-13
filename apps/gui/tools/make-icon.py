# -*- coding: utf-8 -*-
"""生成 GUI 的图标（`src-tauri/icons/icon.ico`）。

为什么要脚本而不是直接塞一个二进制文件：
* `tauri-build` 在 Windows 上**必须**有 `icons/icon.ico`，否则直接构建失败；
* 二进制文件在代码审查里看不见内容、也没法改；
* 而且这个仓库的约定是「能算出来的东西就不要固化」。

图案本身画的就是这个工具的主视图：一块 treemap —— 顶部标题栏 + 一大块 +
右侧切分的小块。尺寸给 16 / 32 / 48 / 256 四档，Windows 按需挑。

用法：python apps/gui/tools/make-icon.py
"""

import os
import struct

# 只画 32×32，其余档位用同一张图放大 —— 图标很小，多档位主要是让
# 任务栏/资源管理器/Alt-Tab 各自取到整数倍尺寸而不发虚。
SIZES = (16, 32, 48, 256)


def pixel(x: int, y: int, size: int):
    """返回一个像素 (B, G, R, A)。图案 = 一块 treemap。"""
    s = size
    pad = max(1, s // 16)                 # 外边框
    header = max(2, s // 4)               # 目录标题栏高度

    if x < pad or y < pad or x >= s - pad or y >= s - pad:
        return (0x3A, 0x2A, 0x1E, 0xFF)   # 深色描边
    if y < pad + header:
        return (0xC0, 0x88, 0x30, 0xFF)   # 标题栏（蓝）
    # 主块 / 右侧两块，用同色系深浅区分「大目录 + 子块」
    split = pad + int((s - 2 * pad) * 0.56)
    if x < split:
        return (0xE0, 0xC0, 0x50, 0xFF)
    mid = pad + header + (s - 2 * pad - header) // 2
    if y < mid:
        return (0x90, 0xF0, 0x70, 0xFF)
    return (0x60, 0xB0, 0x40, 0xFF)


def bmp_image(size: int) -> bytes:
    """一张 32bpp 的 DIB（BITMAPINFOHEADER + XOR 位图 + AND 掩码）。"""
    xor = bytearray()
    for y in range(size - 1, -1, -1):     # DIB 自下而上
        for x in range(size):
            b, g, r, a = pixel(x, y, size)
            xor += bytes((b, g, r, a))
    # 1bpp 掩码：全 0。不透明度交给 alpha 通道，避免 1bpp 锯齿。
    row_bytes = ((size + 31) // 32) * 4
    and_mask = bytes(row_bytes * size)
    # biHeight 要写**两倍** —— 这是 ICO 里的历史约定（XOR 位图 + AND 掩码叠在一起）
    hdr = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    return hdr + bytes(xor) + and_mask


def build() -> bytes:
    images = [bmp_image(s) for s in SIZES]
    icondir = struct.pack("<HHH", 0, 1, len(SIZES))
    offset = 6 + 16 * len(SIZES)
    entries = bytearray()
    for size, img in zip(SIZES, images):
        # 宽高用 1 字节，256 要写成 0
        b = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", b, b, 0, 0, 1, 32, len(img), offset)
        offset += len(img)
    return bytes(icondir + entries + b"".join(images))


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    out_dir = os.path.join(here, "..", "src-tauri", "icons")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "icon.ico")
    data = build()
    with open(out, "wb") as f:
        f.write(data)

    # 自校验：ICO 头 + 每个目录项声明的偏移/长度都要落在文件内
    assert data[:6] == b"\x00\x00\x01\x00\x04\x00", "ICO 头不对"
    for i in range(len(SIZES)):
        e = 6 + 16 * i
        size, off = struct.unpack_from("<II", data, e + 8)
        assert off + size <= len(data), "第 %d 个图像越界" % i
    print("已写出 %s（%d 字节，%d 档尺寸）" % (out, len(data), len(SIZES)))


if __name__ == "__main__":
    main()
