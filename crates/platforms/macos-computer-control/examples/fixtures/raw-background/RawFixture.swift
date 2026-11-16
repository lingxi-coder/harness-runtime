import AppKit
import Darwin
// SkyLight is loaded once for read-only metadata. No Space creation/deletion or
// move API is called here; the owned NSWindow fullscreen lifecycle owns its Space.
final class Spaces {
 static let shared=Spaces()
 private let handle:UnsafeMutableRawPointer?
 private let cid:Int32
 typealias Connection = @convention(c)()->Int32
 typealias Active = @convention(c)(Int32)->UInt64
 typealias Kind = @convention(c)(Int32,UInt64)->Int32
 typealias Memberships = @convention(c)(Int32,Int32,CFArray)->Unmanaged<CFArray>?
 typealias Topology = @convention(c)(Int32)->Unmanaged<CFArray>?
 init(){
  handle=dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight",RTLD_NOW)
  if let handle=handle,let symbol=dlsym(handle,"SLSMainConnectionID") {cid=unsafeBitCast(symbol,to:Connection.self)()}else{cid=0}
 }
 func state(window:Int?=nil)->[String:Any]{
  guard cid>0,let handle=handle,let activeSymbol=dlsym(handle,"SLSGetActiveSpace"),let kindSymbol=dlsym(handle,"SLSSpaceGetType"),let topologySymbol=dlsym(handle,"SLSCopyManagedDisplaySpaces") else{return ["error":"Space query API unavailable","screenLocked":true]}
  let active=unsafeBitCast(activeSymbol,to:Active.self)(cid)
  let kind=unsafeBitCast(kindSymbol,to:Kind.self)(cid,active)
  let topology=unsafeBitCast(topologySymbol,to:Topology.self)(cid)?.takeRetainedValue() as? [[String:Any]] ?? []
  var ids=Set<UInt64>()
  for display in topology {for space in display["Spaces"] as? [[String:Any]] ?? [] {if let id=(space["ManagedSpaceID"] ?? space["id64"]) as? NSNumber {ids.insert(id.uint64Value)}}}
  let session=CGSessionCopyCurrentDictionary() as? [String:Any]
  var value:[String:Any]=["activeSpace":active,"activeSpaceType":kind,"spaceIDs":ids.sorted(),"screenLocked":session==nil || (session?["CGSSessionScreenIsLocked"] as? Bool ?? false)]
  if let window=window,let symbol=dlsym(handle,"SLSCopySpacesForWindows") {
   let windows=NSArray(object:NSNumber(value:window));let query=unsafeBitCast(symbol,to:Memberships.self)
   value["spaces"]=(query(cid,7,unsafeBitCast(windows,to:CFArray.self))?.takeRetainedValue() as? [NSNumber] ?? []).map{$0.uint64Value}
  }
  return value
 }
}

final class Canvas: NSView {
 var downs=0,drags=0,ups=0
 override var acceptsFirstResponder: Bool { true }
 override func acceptsFirstMouse(for event:NSEvent?)->Bool { true }
 override func mouseDown(with event:NSEvent){downs += 1}
 override func mouseDragged(with event:NSEvent){drags += 1}
 override func mouseUp(with event:NSEvent){ups += 1}
 override func draw(_ r:NSRect){NSColor.systemTeal.setFill();bounds.fill()}
}
final class FixtureWindow:NSWindow {
 override var canBecomeKey: Bool { true }
 override var canBecomeMain: Bool { true }
}
final class Fixture:NSObject,NSApplicationDelegate,NSWindowDelegate {
 let root:URL;let shadow:Bool;var window:NSWindow!;let canvas=Canvas();let text=NSTextView();var activations=0;var primary:NSWindow?;var phase="ready";var observer:NSObjectProtocol?;var keysDown=0;var keysUp=0;var lastKey:[String:Any]=[:]
 init(_ p:String,_ s:Bool){root=URL(fileURLWithPath:p);shadow=s}
 func applicationDidFinishLaunching(_ n:Notification){
  NSEvent.addLocalMonitorForEvents(matching:[.leftMouseDown,.leftMouseUp,.leftMouseDragged,.keyDown,.keyUp]) { e in
    if e.type == .keyDown || e.type == .keyUp {
      if e.type == .keyDown {self.keysDown += 1} else {self.keysUp += 1}
      self.lastKey=["code":e.keyCode,"window":e.windowNumber,"flags":e.modifierFlags.rawValue,"type":e.type.rawValue]
      self.persist()
      return e
    }
    fputs("mouse type=\(e.type.rawValue) window=\(e.windowNumber) local=\(e.locationInWindow)\n",stderr)
    return e
  }
  NSApp.setActivationPolicy(.regular)
  window=FixtureWindow(contentRect:NSRect(x:shadow ? 850:150,y:200,width:500,height:300),styleMask:[.titled,.closable],backing:.buffered,defer:false)
  window.title=shadow ? "Owned raw probe shadow":"Owned raw probe target";window.isReleasedWhenClosed=false
  canvas.frame=NSRect(x:20,y:20,width:460,height:120);text.frame=NSRect(x:20,y:170,width:460,height:80);text.string="seed"
  window.contentView!.addSubview(canvas);window.contentView!.addSubview(text)
  let menu=NSMenu();let appItem=NSMenuItem(title:"Owned fixture",action:nil,keyEquivalent:"");appItem.submenu=NSMenu(title:"Owned fixture");menu.addItem(appItem);let edit=NSMenu(title:"Edit");let select=NSMenuItem(title:"Select All",action:#selector(NSText.selectAll(_:)),keyEquivalent:"a");select.keyEquivalentModifierMask=[.command];select.target=text;edit.addItem(select);let editItem=NSMenuItem(title:"Edit",action:nil,keyEquivalent:"");editItem.submenu=edit;menu.addItem(editItem);NSApp.mainMenu=menu
  window.orderFront(nil);window.makeFirstResponder(text)
  if shadow {window.makeKeyAndOrderFront(nil);NSApp.activate(ignoringOtherApps:true)}
  observer=NSWorkspace.shared.notificationCenter.addObserver(forName:NSWorkspace.didActivateApplicationNotification,object:nil,queue:.main){_ in self.activations += 1;self.persist()}
  Timer.scheduledTimer(withTimeInterval:0.05,repeats:true){_ in self.persist()};persist()
  if CommandLine.arguments.contains("space-source") {
    phase="entering-fullscreen"
    window.collectionBehavior=[.fullScreenAuxiliary]
    let p=NSWindow(contentRect:NSRect(x:200,y:150,width:600,height:400),styleMask:[.titled,.closable,.resizable],backing:.buffered,defer:false)
    p.title="Owned temporary fullscreen source";p.isReleasedWhenClosed=false;p.collectionBehavior=[.fullScreenPrimary];p.delegate=self;primary=p
    p.makeKeyAndOrderFront(nil);NSApp.activate(ignoringOtherApps:true);p.toggleFullScreen(nil)
  }
 }
 func windowDidEnterFullScreen(_ n:Notification){
   let previous=window!;let content=previous.contentView!;previous.contentView=nil;previous.close()
   window=FixtureWindow(contentRect:NSRect(x:150,y:200,width:500,height:300),styleMask:[.titled,.closable],backing:.buffered,defer:false)
   window.title="Owned movable auxiliary target";window.isReleasedWhenClosed=false;window.collectionBehavior=[.fullScreenAuxiliary];window.contentView=content
   window.makeKeyAndOrderFront(nil);window.makeMain();window.makeKey();window.makeFirstResponder(text);phase="space-ready";persist()
 }
 func spaceState()->[String:Any]{Spaces.shared.state(window:window.windowNumber)}
 func persist(){let rect=window.frame;let loc=CGEvent(source:nil)!.location
  func textPoint(_ index:Int)->[Double]{
    let glyph=text.layoutManager!.boundingRect(forGlyphRange:NSRange(location:index,length:1),in:text.textContainer!)
    let p=text.convert(NSPoint(x:glyph.minX+text.textContainerInset.width+1,y:glyph.midY+text.textContainerInset.height),to:nil)
    return [p.x,rect.height-p.y]
  }

  var d:[String:Any]=["phase":phase,"pid":ProcessInfo.processInfo.processIdentifier,"window":window.windowNumber,"frame":[rect.minX,rect.minY,rect.width,rect.height],"canvasScreen": [rect.minX+100,NSScreen.screens[0].frame.height-(rect.minY+80)],"canvasLocal":[100,80],"downs":canvas.downs,"drags":canvas.drags,"ups":canvas.ups,"text":text.string,"selection":[text.selectedRange().location,text.selectedRange().length],"frontmost":NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1,"activations":activations,"cursor":[loc.x,loc.y],"firstResponder":String(describing:type(of:window.firstResponder!)),"textStartWindow":textPoint(0),"textEndWindow":textPoint(3)]
  d["keysDown"]=keysDown;d["keysUp"]=keysUp;d["lastKey"]=lastKey
  d["keyWindow"]=NSApp.keyWindow.map{$0.windowNumber} as Any? ?? NSNull()
  d["mainWindow"]=NSApp.mainWindow.map{$0.windowNumber} as Any? ?? NSNull()
  d["windowIsKey"]=window.isKeyWindow;d["windowIsMain"]=window.isMainWindow
  d.merge(spaceState()){_,v in v}
  try? JSONSerialization.data(withJSONObject:d,options:[.sortedKeys]).write(to:root,options:.atomic)
 }
}
func emit(_ value:Any){
 guard let data=try? JSONSerialization.data(withJSONObject:value,options:[.sortedKeys]) else {exit(2)}
 FileHandle.standardOutput.write(data);FileHandle.standardOutput.write(Data([10]))
}
let args=CommandLine.arguments
if args.contains("--probe-state") {emit(Spaces.shared.state());exit(0)}
if args.contains("--running-fixtures") {
 emit(NSWorkspace.shared.runningApplications.filter{$0.processIdentifier != ProcessInfo.processInfo.processIdentifier && ["org.lingxi.raw-probe.target","org.lingxi.raw-probe.shadow"].contains($0.bundleIdentifier ?? "")}.map{["bundleId":$0.bundleIdentifier ?? "","pid":$0.processIdentifier,"path":$0.executableURL?.resolvingSymlinksInPath().path ?? ""] as [String:Any]});exit(0)
}
if args.count==4 && args[1]=="--activate-owned-shadow" {
 guard let pid=Int32(args[2]),let app=NSRunningApplication(processIdentifier:pid),app.bundleIdentifier=="org.lingxi.raw-probe.shadow",app.executableURL?.resolvingSymlinksInPath().path==args[3] else {exit(2)}
 // Fixture preparation only: reveal this exact owned window across Spaces.
 emit(["activated":app.activate(options:[.activateAllWindows])]);exit(0)
}
guard args.count>=2 else {fputs("expected output JSON path or read-only --probe-state/--running-fixtures\n",stderr);exit(2)}
let fixture=Fixture(args[1],args.contains("shadow"));NSApplication.shared.delegate=fixture;NSApplication.shared.run()
