"""Verify the built PE icon without running the executable (Windows only)."""
import ctypes
import pathlib
import struct
import sys
import unittest
from ctypes import wintypes


class IconResourceTest(unittest.TestCase):
    def test_executable_embeds_all_ico_images(self):
        exe = pathlib.Path(sys.argv[1]).resolve()
        icon_path = pathlib.Path(__file__).parents[1] / "assets" / "fast365.ico"
        self.assertTrue(icon_path.is_file(), "multiresolution icon is missing")
        ico = icon_path.read_bytes()
        reserved, kind, count = struct.unpack_from("<HHH", ico)
        self.assertEqual((reserved, kind), (0, 1))
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.LoadLibraryExW.argtypes = [wintypes.LPCWSTR, wintypes.HANDLE, wintypes.DWORD]
        kernel.LoadLibraryExW.restype = wintypes.HMODULE
        kernel.FindResourceW.argtypes = [wintypes.HMODULE, ctypes.c_void_p, ctypes.c_void_p]
        kernel.FindResourceW.restype = ctypes.c_void_p
        kernel.LoadResource.argtypes = [wintypes.HMODULE, ctypes.c_void_p]
        kernel.LoadResource.restype = ctypes.c_void_p
        kernel.LockResource.argtypes = [ctypes.c_void_p]
        kernel.LockResource.restype = ctypes.c_void_p
        kernel.SizeofResource.argtypes = [wintypes.HMODULE, ctypes.c_void_p]
        kernel.SizeofResource.restype = wintypes.DWORD
        kernel.FreeLibrary.argtypes = [wintypes.HMODULE]
        module = kernel.LoadLibraryExW(str(exe), None, 0x22)
        self.assertTrue(module, "cannot load PE as data")
        def resource(name, kind):
            handle = kernel.FindResourceW(module, name, kind)
            self.assertTrue(handle, f"missing resource {kind}/{name}")
            size = kernel.SizeofResource(module, handle)
            data = kernel.LockResource(kernel.LoadResource(module, handle))
            self.assertTrue(data)
            return ctypes.string_at(data, size)
        try:
            group = resource(1, 14)
            self.assertEqual(struct.unpack_from("<HHH", group), (0, 1, count))
            sizes = set()
            for i in range(count):
                width, height, colors, reserved, planes, bits, length, offset = struct.unpack_from("<BBBBHHII", ico, 6 + 16 * i)
                gw, gh, gc, gr, gp, gb, gl, image_id = struct.unpack_from("<BBBBHHIH", group, 6 + 14 * i)
                # The resource compiler normalizes PNG ICO planes from 0 to 1.
                self.assertEqual((gw, gh, gc, gr, gb, gl), (width, height, colors, reserved, bits, length))
                self.assertIn(gp, (planes, 1) if planes == 0 else (planes,))
                self.assertEqual(resource(image_id, 3), ico[offset:offset + length])
                sizes.add(width or 256)
            self.assertEqual(sizes, {16, 24, 32, 48, 64, 128, 256})
        finally:
            kernel.FreeLibrary(module)


if __name__ == "__main__":
    exe_arg = sys.argv.pop(1)
    sys.argv.append(exe_arg)
    unittest.main(argv=[sys.argv[0]])
