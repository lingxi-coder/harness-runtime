import AppKit
import ImageIO
import CryptoKit
import Foundation

// Independent reconstruction of the observed native encoder call sequence.
// The input PNG boundary deliberately excludes capture/resize pixel parity.
guard CommandLine.arguments.count == 2 else { fatalError("supply the synthetic PNG directory") }
let root = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
var cases: [[String: Any]] = []
func sha(_ bytes: Data) -> String { SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined() }
for url in try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil).filter({ $0.pathExtension == "png" }).sorted(by: { $0.path < $1.path }) {
    try autoreleasepool {
        let png = try Data(contentsOf: url)
        guard let source = CGImageSourceCreateWithData(png as CFData, nil),
              let cgImage = CGImageSourceCreateImageAtIndex(source, 0, nil) else { fatalError("fixture PNG decode failed") }
        let image = NSImage(cgImage: cgImage, size: NSSize(width: cgImage.width, height: cgImage.height))
        guard let tiff = image.tiffRepresentation,
              let bitmap = NSBitmapImageRep(data: tiff),
              let jpeg = bitmap.representation(using: .jpeg, properties: [.compressionFactor: 0.75]) else { fatalError("AppKit reference encoding failed") }
        try jpeg.write(to: url.deletingPathExtension().appendingPathExtension("jpg"))
        let directBitmap = NSBitmapImageRep(cgImage: cgImage)
        guard let windowJPEG = directBitmap.representation(using: .jpeg, properties: [.compressionFactor: 0.75]) else { fatalError("window bitmap reference") }
        try windowJPEG.write(to: url.deletingPathExtension().appendingPathExtension("bitmap.jpg"))
        cases.append(["name": url.lastPathComponent, "width": cgImage.width, "height": cgImage.height,
                      "pngSha256": sha(png), "jpegSha256": sha(jpeg), "jpegBytes": jpeg.count, "windowJpegSha256": sha(windowJPEG), "windowJpegBytes":windowJPEG.count])
    }
}
let receipt: [String: Any] = ["referenceVersion": "2.1.325", "macOS": ProcessInfo.processInfo.operatingSystemVersionString,
    "quality": 0.75, "pipeline": ["ImageIO PNG decode", "NSImage(cgImage:size actual pixel dimensions)", "TIFFRepresentation", "NSBitmapImageRep(data:)", "JPEG compressionFactor 0.75"],
    "cases": cases, "limits": ["Independent same-host AppKit pipeline reconstruction; not execution of the proprietary native continuation", "PNG boundary excludes ScreenCaptureKit capture/resize/color-space equivalence"]]
try JSONSerialization.data(withJSONObject: receipt, options: [.prettyPrinted, .sortedKeys]).write(to: root.appendingPathComponent("reference.json"))
print("Encoded \(cases.count) independent AppKit fixtures")
