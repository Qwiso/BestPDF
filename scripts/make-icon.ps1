# Builds assets/best-pdf.ico from the artwork in assets/icon-source.webp.
# Removes the solid backdrop (flood fill from the edges, so the same color
# inside the artwork is kept), crops to the artwork, pads to square, and writes
# a multi-resolution ICO with PNG-compressed frames.
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
$Source = Join-Path $Root 'assets\icon-source.webp'
$Out = Join-Path $Root 'assets\best-pdf.ico'
$Sizes = 16, 20, 24, 32, 40, 48, 64, 96, 128, 256

Add-Type -AssemblyName PresentationCore, System.Drawing
Add-Type -TypeDefinition @'
using System; using System.Collections.Generic; using System.Linq;
public static class IconPrep {
    // Largest per-channel difference from the backdrop color.
    static int Dist(byte[] px, int i, byte[] bg) {
        return Math.Max(Math.Abs(px[i*4] - bg[0]), Math.Max(Math.Abs(px[i*4+1] - bg[1]), Math.Abs(px[i*4+2] - bg[2])));
    }

    // BGRA pixels in place: the backdrop (the corner pixel's color, flood-filled
    // from the border so matching colors inside the artwork are kept) becomes
    // transparent, and the anti-aliased rim is un-blended into partial alpha.
    public static void ClearBackdrop(byte[] px, int w, int h, int tolerance) {
        var bg = new[] { px[0], px[1], px[2] };
        var seen = new bool[w * h];
        var stack = new Stack<int>();
        Func<int, bool> isBg = i => Dist(px, i, bg) <= tolerance;
        for (int x = 0; x < w; x++) { stack.Push(x); stack.Push((h - 1) * w + x); }
        for (int y = 0; y < h; y++) { stack.Push(y * w); stack.Push(y * w + w - 1); }
        while (stack.Count > 0) {
            int i = stack.Pop();
            if (seen[i] || !isBg(i)) continue;
            seen[i] = true;
            px[i*4+3] = 0;
            int x = i % w, y = i / w;
            if (x > 0) stack.Push(i - 1);
            if (x < w - 1) stack.Push(i + 1);
            if (y > 0) stack.Push(i - w);
            if (y < h - 1) stack.Push(i + w);
        }
        // Rim: pixels within 2px of the backdrop are a blend of artwork and
        // backdrop. Estimate coverage from distance and recover the color.
        var rim = new List<int>();
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
            int i = y * w + x;
            if (seen[i]) continue;
            bool near = false;
            for (int dy = -2; dy <= 2 && !near; dy++) for (int dx = -2; dx <= 2 && !near; dx++) {
                int nx = x + dx, ny = y + dy;
                if (nx >= 0 && ny >= 0 && nx < w && ny < h && seen[ny * w + nx]) near = true;
            }
            if (near) rim.Add(i);
        }
        foreach (int i in rim) {
            int a = Math.Min(255, Dist(px, i, bg) * 255 / 200);
            if (a >= 255) continue;
            for (int c = 0; c < 3; c++)
                px[i*4+c] = (byte)Math.Max(0, Math.Min(255, bg[c] + (px[i*4+c] - bg[c]) * 255 / Math.Max(1, a)));
            px[i*4+3] = (byte)a;
        }
    }
    // Keeps only the largest connected shape; clears compression specks.
    public static void KeepLargestShape(byte[] px, int w, int h) {
        var label = new int[w * h];
        var sizes = new List<int> { 0 };
        var stack = new Stack<int>();
        for (int start = 0; start < w * h; start++) {
            if (label[start] != 0 || px[start*4+3] == 0) continue;
            int id = sizes.Count, size = 0;
            stack.Push(start); label[start] = id;
            while (stack.Count > 0) {
                int i = stack.Pop(); size++;
                int x = i % w, y = i / w;
                foreach (int n in new[] { x > 0 ? i - 1 : -1, x < w - 1 ? i + 1 : -1, y > 0 ? i - w : -1, y < h - 1 ? i + w : -1 })
                    if (n >= 0 && label[n] == 0 && px[n*4+3] != 0) { label[n] = id; stack.Push(n); }
            }
            sizes.Add(size);
        }
        int keep = sizes.IndexOf(sizes.Max());
        for (int i = 0; i < w * h; i++) if (label[i] != keep) px[i*4+3] = 0;
    }

    // Bounding box of opaque pixels: x, y, width, height.
    public static int[] Bounds(byte[] px, int w, int h) {
        int x0 = w, y0 = h, x1 = -1, y1 = -1;
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++)
            if (px[(y * w + x) * 4 + 3] != 0) { x0 = Math.Min(x0, x); y0 = Math.Min(y0, y); x1 = Math.Max(x1, x); y1 = Math.Max(y1, y); }
        return new[] { x0, y0, x1 - x0 + 1, y1 - y0 + 1 };
    }
}
'@

# Decode (WIC handles WebP) into BGRA bytes.
$frame = [Windows.Media.Imaging.BitmapDecoder]::Create([Uri]$Source, 'None', 'OnLoad').Frames[0]
$bgra = New-Object Windows.Media.Imaging.FormatConvertedBitmap($frame, [Windows.Media.PixelFormats]::Bgra32, $null, 0)
$w = $bgra.PixelWidth; $h = $bgra.PixelHeight
$px = New-Object byte[] ($w * $h * 4)
$bgra.CopyPixels($px, $w * 4, 0)
[IconPrep]::ClearBackdrop($px, $w, $h, 24)
[IconPrep]::KeepLargestShape($px, $w, $h)
$b = [IconPrep]::Bounds($px, $w, $h)

# Artwork into a square transparent canvas, centered with a small margin.
$full = New-Object Drawing.Bitmap $w, $h, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
$data = $full.LockBits((New-Object Drawing.Rectangle 0, 0, $w, $h), 'WriteOnly', $full.PixelFormat)
[Runtime.InteropServices.Marshal]::Copy($px, 0, $data.Scan0, $px.Length)
$full.UnlockBits($data)
$side = [int]([Math]::Max($b[2], $b[3]) * 1.04)
$square = New-Object Drawing.Bitmap $side, $side, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [Drawing.Graphics]::FromImage($square)
$g.DrawImage($full, [int](($side - $b[2]) / 2), [int](($side - $b[3]) / 2), (New-Object Drawing.Rectangle $b[0], $b[1], $b[2], $b[3]), 'Pixel')
$g.Dispose(); $full.Dispose()

# One PNG per size.
$pngs = foreach ($s in $Sizes) {
    $bmp = New-Object Drawing.Bitmap $s, $s, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'; $g.CompositingQuality = 'HighQuality'
    $attr = New-Object Drawing.Imaging.ImageAttributes; $attr.SetWrapMode('TileFlipXY')
    $g.DrawImage($square, (New-Object Drawing.Rectangle 0, 0, $s, $s), 0, 0, $side, $side, 'Pixel', $attr)
    $g.Dispose()
    $ms = New-Object IO.MemoryStream; $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    , $ms.ToArray()
}
$square.Dispose()

# ICO container: header, directory, then the PNG blobs.
$ico = New-Object IO.MemoryStream
$wr = New-Object IO.BinaryWriter $ico
$wr.Write([uint16]0); $wr.Write([uint16]1); $wr.Write([uint16]$Sizes.Count)
$offset = 6 + 16 * $Sizes.Count
for ($i = 0; $i -lt $Sizes.Count; $i++) {
    $s = $Sizes[$i]; $dim = if ($s -ge 256) { 0 } else { $s }
    $wr.Write([byte]$dim); $wr.Write([byte]$dim); $wr.Write([byte]0); $wr.Write([byte]0)
    $wr.Write([uint16]1); $wr.Write([uint16]32)
    $wr.Write([uint32]$pngs[$i].Length); $wr.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $wr.Write($p) }
[IO.File]::WriteAllBytes($Out, $ico.ToArray())
"$Out  ($([math]::Round($ico.Length / 1KB)) KB, sizes: $($Sizes -join ', '))"
