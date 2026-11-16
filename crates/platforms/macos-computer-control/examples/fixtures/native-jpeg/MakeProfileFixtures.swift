import Foundation
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers
guard CommandLine.arguments.count == 2 else { fatalError("supply the synthetic PNG output directory") }
let root=URL(fileURLWithPath:CommandLine.arguments[1],isDirectory:true)
for (name,spaceName,alpha) in [("icc-srgb",CGColorSpace.sRGB,false),("icc-display-p3",CGColorSpace.displayP3,false),("icc-display-p3-alpha",CGColorSpace.displayP3,true)] {
 let w=65,h=47
 var bytes=[UInt8](repeating:0,count:w*h*4)
 for y in 0..<h { for x in 0..<w {
  let a=alpha ? (x*7+y*13)%256:255
  let i=(y*w+x)*4
  bytes[i]=UInt8(((x*19+y*3)%256)*a/255)
  bytes[i+1]=UInt8(((x*5+y*29)%256)*a/255)
  bytes[i+2]=UInt8(((x*31+y*11)%256)*a/255)
  bytes[i+3]=UInt8(a)
 }}
 guard let space=CGColorSpace(name:spaceName),let provider=CGDataProvider(data:Data(bytes) as CFData),let image=CGImage(width:w,height:h,bitsPerComponent:8,bitsPerPixel:32,bytesPerRow:w*4,space:space,bitmapInfo:CGBitmapInfo(rawValue:CGImageAlphaInfo.premultipliedLast.rawValue),provider:provider,decode:nil,shouldInterpolate:false,intent:.defaultIntent),let destination=CGImageDestinationCreateWithURL(root.appendingPathComponent(name+".png") as CFURL,UTType.png.identifier as CFString,1,nil) else {fatalError("profile fixture")}
 CGImageDestinationAddImage(destination,image,nil)
 guard CGImageDestinationFinalize(destination) else {fatalError("profile PNG")}
}
print("Generated sRGB/P3/alpha ICC fixtures")
