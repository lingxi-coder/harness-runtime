// Independent reconstruction of the hash-pinned native display SCK parameters.
// Only owned fixture windows are permitted into reference images.
import AppKit
import ScreenCaptureKit
import CoreGraphics
import Foundation
import CryptoKit
import Darwin
guard CommandLine.arguments.count == 2 else { fatalError("supply the controlled reference directory") }
let root=URL(fileURLWithPath:CommandLine.arguments[1],isDirectory:true)
let config=try JSONSerialization.jsonObject(with:Data(contentsOf:root.appendingPathComponent("config.json"))) as! [String:Any]
let displayID=(config["displayId"] as! NSNumber).uint32Value
let rootID=config["hostRoot"] as! String
let target=config["allowedBundleId"] as! String
let foreign=config["foreignBundleId"] as! String
guard target == "org.lingxi.raw-probe.target", foreign == "org.lingxi.raw-probe.shadow" else {
 fatalError("only the disposable raw-background fixture bundles are permitted")
}
let chrome:Set<String>=["com.apple.dock","com.apple.wallpaper.agent","com.apple.controlcenter","com.apple.SystemUIServer","com.apple.TextInputSwitcher","com.apple.wifi.WiFiAgent","com.apple.AccessibilityUIServer","com.apple.loginwindow"]
func budget(_ w:Int,_ h:Int)->(Int,Int){
 func tiles(_ w:Int,_ h:Int)->Int{((w+27)/28)*((h+27)/28)}
 if w<=1568 && h<=1568 && tiles(w,h)<=1568{return(w,h)}
 if h>w{let(a,b)=budget(h,w);return(b,a)}
 let ratio=Double(w)/Double(h);var low=1,high=w
 while low+1 != high {let mid=low+(high-low)/2;let height=max(1,Int((Double(mid)/ratio).rounded()));if mid<=1568 && tiles(mid,height)<=1568{low=mid}else{high=mid}}
 return(low,max(1,Int((Double(low)/ratio).rounded())))
}
func resolve(_ original:pid_t)->String?{
 var pid=original
 for _ in 0..<8 {
  if pid<2{return nil}
  if let id=NSRunningApplication(processIdentifier:pid)?.bundleIdentifier{return id}
  var b=[UInt8](repeating:0,count:136)
  let count=b.withUnsafeMutableBytes{proc_pidinfo(pid,3,0,$0.baseAddress,136)}
  if count != 136{return nil}
  pid=pid_t(UInt32(b[16]) | UInt32(b[17])<<8 | UInt32(b[18])<<16 | UInt32(b[19])<<24)
 }
 return nil
}
func excluded(_ app:SCRunningApplication,_ allowed:Set<String>)->Bool{
 let id=app.bundleIdentifier
 if id==rootID || id.hasPrefix(rootID+"."){return true}
 if id.isEmpty {
  if NSRunningApplication(processIdentifier:app.processID)==nil{return false}
  guard let resolved=resolve(app.processID) else{return true}
  return !allowed.contains(resolved)
 }
 if allowed.contains(id)||chrome.contains(id){return false}
 guard let app=NSRunningApplication(processIdentifier:app.processID) else{return true}
 return app.activationPolicy != .prohibited
}
func encode(_ image:CGImage)->Data{
 let ns=NSImage(cgImage:image,size:NSSize(width:image.width,height:image.height))
 let bitmap=NSBitmapImageRep(data:ns.tiffRepresentation!)!
 return bitmap.representation(using:.jpeg,properties:[.compressionFactor:0.75])!
}
func sha(_ bytes:Data)->String{SHA256.hash(data:bytes).map{String(format:"%02x",$0)}.joined()}
Task { @MainActor in
 do {
  let content=try await SCShareableContent.excludingDesktopWindows(false,onScreenWindowsOnly:false)
  guard let display=content.displays.first(where:{$0.displayID==displayID}) else{fatalError("selected owned display absent")}
  let pw=Int(CGDisplayPixelsWide(displayID)),ph=Int(CGDisplayPixelsHigh(displayID))
  let (fw,fh)=budget(pw,ph)
  let logical=config["logicalRegion"] as! [Double]
  let region=[logical[0]/display.frame.width*Double(fw),logical[1]/display.frame.height*Double(fh),logical[2]/display.frame.width*Double(fw),logical[3]/display.frame.height*Double(fh)]
  let dx=display.frame.width/Double(fw),dy=display.frame.height/Double(fh)
  let crop=CGRect(x:region[0]*dx,y:region[1]*dy,width:(region[2]-region[0])*dx,height:(region[3]-region[1])*dy)
  let (zw,zh)=budget(Int((crop.width*Double(pw)/display.frame.width).rounded()),Int((crop.height*Double(ph)/display.frame.height).rounded()))
  func capture(_ allowed:Set<String>,_ rect:CGRect?,_ w:Int,_ h:Int) async throws->CGImage {
   let filter=SCContentFilter(display:display,excludingApplications:content.applications.filter{excluded($0,allowed)},exceptingWindows:[])
   let cfg=SCStreamConfiguration();cfg.width=w;cfg.height=h;cfg.showsCursor=false;cfg.capturesAudio=false
   if let rect=rect{cfg.sourceRect=rect;cfg.scalesToFit=true}
   return try await SCScreenshotManager.captureImage(contentFilter:filter,configuration:cfg)
  }
  let image=try await capture([target],nil,fw,fh)
  let withForeign=try await capture([target,foreign],nil,fw,fh)
  let zoom=try await capture([target],crop,zw,zh)
  var output=config;output["region"]=region
  try JSONSerialization.data(withJSONObject:output,options:[.prettyPrinted,.sortedKeys]).write(to:root.appendingPathComponent("config.json"))
  var cases=[[String:Any]]()
  for (name,cg) in [("screenshot",image),("with-foreign",withForeign),("zoom",zoom)]{
   let data=encode(cg);try data.write(to:root.appendingPathComponent(name+".jpg"))
   cases.append(["name":name,"width":cg.width,"height":cg.height,"jpegSha256":sha(data),"bytes":data.count,"colorSpace":cg.colorSpace?.name as String? ?? "unknown"])
  }
  try JSONSerialization.data(withJSONObject:["reference":"2.1.325 independent SCK/AppKit reconstruction","cases":cases,"sourceRect":[crop.minX,crop.minY,crop.width,crop.height],"scope":"owned fixtures only; static zoom full bytes; screenshot stable owned/foreign marker pixel patches"],options:[.prettyPrinted,.sortedKeys]).write(to:root.appendingPathComponent("reference.json"))
  print("Generated independent owned-fixture SCK references")
  CFRunLoopStop(CFRunLoopGetMain())
 }catch{fputs("SCK reference failed: \(error)\n",stderr);exit(1)}
}
CFRunLoopRun()
