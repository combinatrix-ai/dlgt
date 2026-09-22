import AppKit
import ApplicationServices
import CryptoKit
import Darwin

struct Failure: Error { let message: String }
func require(_ ok: Bool, _ message: String) throws { if !ok { throw Failure(message: message) } }
func attr(_ e: AXUIElement, _ key: String) -> CFTypeRef? { var v: CFTypeRef?; return AXUIElementCopyAttributeValue(e, key as CFString, &v) == .success ? v : nil }
func str(_ e: AXUIElement, _ key: String) -> String { if let s = attr(e,key) as? String { return s }; if let n = attr(e,key) as? NSNumber { return n.stringValue }; if let u = attr(e,key) as? URL { return u.absoluteString }; return "" }
func kids(_ e: AXUIElement, _ key: String = kAXChildrenAttribute) -> [AXUIElement] { attr(e,key) as? [AXUIElement] ?? [] }
func emit(_ value: Any) throws { let data = try JSONSerialization.data(withJSONObject:value,options:[.sortedKeys]); FileHandle.standardOutput.write(data); print("") }
func run(_ req: [String:Any]) throws {
    try require(AXIsProcessTrusted(), "ACCESSIBILITY_REQUIRED: enable Accessibility for the invoking terminal")
    let op = req["op"] as? String ?? "snapshot"
    if op == "open" {
        let path = req["path"] as? String ?? ""
        try require(path.hasPrefix("/"), "absolute folder path required")
        var url = URLComponents(string:"claude://code/new")!
        url.queryItems = [URLQueryItem(name:"folder",value:path)]
        try require(NSWorkspace.shared.open(url.url!), "deep link failed")
        try emit(["opened":true]); return
    }
    let apps = NSRunningApplication.runningApplications(withBundleIdentifier:"com.anthropic.claudefordesktop")
    try require(apps.count == 1, "expected one running Claude application")
    let app = apps[0], root = AXUIElementCreateApplication(app.processIdentifier)
    AXUIElementSetMessagingTimeout(root, 2)
    let windows = kids(root,kAXWindowsAttribute)
    try require(windows.filter { str($0,kAXSubroleAttribute) == "AXStandardWindow" }.count == 1, "expected exactly one Claude window")
    var elements:[AXUIElement]=[], rows:[[String:Any]]=[]
    func walk(_ e: AXUIElement, _ depth:Int, _ parent:Int) throws {
        try require(elements.count < 12000 && depth < 80, "AX tree truncated")
        let id = elements.count; elements.append(e)
        rows.append(["id":id,"parent":parent,"role":str(e,kAXRoleAttribute),"title":str(e,kAXTitleAttribute),"description":str(e,kAXDescriptionAttribute),"value":str(e,kAXValueAttribute),"url":str(e,kAXURLAttribute),"help":str(e,kAXHelpAttribute),"enabled":(attr(e,kAXEnabledAttribute) as? Bool) ?? false])
        for child in kids(e) { try walk(child,depth+1,id) }
    }
    // Native popup menus may be siblings of the window in the app AX tree.
    try walk(root,0,-1)
    let data = try JSONSerialization.data(withJSONObject:rows,options:[.sortedKeys])
    let fingerprint = SHA256.hash(data:data).map{String(format:"%02x",$0)}.joined()
    if op == "snapshot" { try emit(["pid":app.processIdentifier,"fingerprint":fingerprint,"nodes":rows]); return }
    try require(req["pid"] as? Int == Int(app.processIdentifier), "STALE_AX: application changed")
    guard let expected=req["target"] as? [String:Any] else { throw Failure(message:"missing expected target") }
    let keys=["role","title","description","value","help"]
    let matches=rows.indices.filter { index in keys.allSatisfy { (rows[index][$0] as? String) == (expected[$0] as? String) } && rows[index]["enabled"] as? Bool == true }
    try require(matches.count == 1,"STALE_AX: target changed or ambiguous")
    let id=matches[0], target=elements[id], role=str(target,kAXRoleAttribute)
    if let expectedURL=req["url"] as? String {
        let urls=rows.filter { $0["role"] as? String == "AXWebArea" && ($0["url"] as? String ?? "").contains("claude.ai") }.compactMap { $0["url"] as? String }
        try require(urls == [expectedURL],"STALE_AX: conversation changed")
    }
    if op == "set" {
        try require(role == "AXTextArea", "expected composer")
        try require(str(target,kAXValueAttribute) == (req["expected"] as? String ?? ""), "draft changed")
        try require(AXUIElementSetAttributeValue(target,kAXValueAttribute as CFString,(req["text"] as? String ?? "") as CFString) == .success,"AX set failed")
    } else if op == "press" {
        try require(["AXPopUpButton","AXMenuItem","AXButton"].contains(role), "unsupported AX control")
        let label = str(target,kAXTitleAttribute)+str(target,kAXDescriptionAttribute)
        if label.contains("信頼") || label.lowercased().contains("trust") {
            guard let path = req["trusted_path"] as? String, path.hasPrefix("/"), let parentID = rows[id]["parent"] as? Int, rows.indices.contains(parentID) else { throw Failure(message:"workspace authorization missing") }
            var scope:Set<Int>=[parentID]; var paths:[String]=[]
            for row in rows { if let parent=row["parent"] as? Int, scope.contains(parent), let child=row["id"] as? Int { scope.insert(child); if row["role"] as? String == "AXStaticText", let value=row["value"] as? String, value.hasPrefix("/") { paths.append(value) } } }
            try require(paths == [path],"WORKSPACE_MISMATCH: exact requested folder is not in this confirmation")
        }
        try require(AXUIElementPerformAction(target,kAXPressAction as CFString) == .success,"AX press failed")
    } else { throw Failure(message:"unknown operation") }
    try emit(["acted":true])
}
do {
    let data=FileHandle.standardInput.readDataToEndOfFile()
    guard let request=try JSONSerialization.jsonObject(with:data) as? [String:Any] else { throw Failure(message:"invalid request") }
    try run(request)
} catch { try? emit(["error":(error as? Failure)?.message ?? String(describing:error)]); exit(1) }
