import AVFoundation
import Capacitor
import Foundation
import Security
import UIKit
import UserNotifications
import VisionKit

final class AtmanViewController: CAPBridgeViewController {
    override func capacitorDidLoad() {
        bridge?.registerPluginInstance(AtmanDevice())
    }
}

private enum DeviceBridgeError: Error {
    case keychain(OSStatus)
    case invalidRequest
}

private struct DeviceVault {
    var account = "paired-device"
    private var key: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "run.atman.mobile.device",
            kSecAttrAccount as String: account,
            kSecAttrSynchronizable as String: false,
        ]
    }

    func load() throws -> Data? {
        var query = key
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw DeviceBridgeError.keychain(status)
        }
        return data
    }

    func replace(with record: Data?) throws {
        guard let record else {
            let status = SecItemDelete(key as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw DeviceBridgeError.keychain(status)
            }
            return
        }
        guard record.count <= 8192 else { throw DeviceBridgeError.invalidRequest }
        let update = [kSecValueData as String: record]
        var status = SecItemUpdate(key as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var item = key
            item[kSecValueData as String] = record
            item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            status = SecItemAdd(item as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw DeviceBridgeError.keychain(status) }
    }
}

final class AtmanNoticeInbox: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    static let shared = AtmanNoticeInbox()
    static let changed = Notification.Name("atman.notice.open")
    private let queue = DispatchQueue(label: "run.atman.mobile.notice")
    private let vault = DeviceVault(account: "pending-notification")

    private func pending() throws -> [String: Any]? {
        guard let data = try vault.load() else { return nil }
        return try JSONSerialization.jsonObject(with: data) as? [String: Any]
    }

    func accept(_ response: UNNotificationResponse, completion: @escaping () -> Void = {}) {
        guard response.actionIdentifier == UNNotificationDefaultActionIdentifier,
              let id = response.notification.request.content.userInfo["notification_id"] as? String else {
            completion()
            return
        }
        record(id, completion: completion)
    }

    private func record(_ id: String, completion: @escaping () -> Void = {}) {
        guard UUID(uuidString: id)?.uuidString.lowercased() == id, id.dropFirst(14).first == "4" else {
            completion()
            return
        }
        queue.async {
            defer { completion() }
            guard let record = try? DeviceVault().load(),
                  let value = try? JSONSerialization.jsonObject(with: record) as? [String: Any],
                  let device = (value["credentials"] as? [String: Any])?["device"] as? [String: Any],
                  let owner = device["id"] as? String, UUID(uuidString: owner) != nil else { return }
            if let pending = try? self.pending(),
               pending["notification_id"] as? String == id, pending["device_id"] as? String == owner { return }
            let notice = [
                "notification_id": id, "device_id": owner, "ticket": UUID().uuidString.lowercased(),
            ]
            guard let data = try? JSONSerialization.data(withJSONObject: notice),
                  (try? self.vault.replace(with: data)) != nil else { return }
            DispatchQueue.main.async { NotificationCenter.default.post(name: Self.changed, object: nil) }
        }
    }

    func peek(_ reply: @escaping (Result<[String: Any]?, Error>) -> Void) {
        queue.async { reply(Result { try self.pending() }) }
    }

    func acknowledge(_ ticket: String, completion: @escaping (Result<Void, Error>) -> Void) {
        queue.async {
            completion(Result {
                if try self.pending()?["ticket"] as? String == ticket { try self.vault.replace(with: nil) }
            })
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        accept(response, completion: completionHandler)
    }
}

@objc(AtmanDevice)
final class AtmanDevice: CAPPlugin, CAPBridgedPlugin {
    let identifier = "AtmanDevice"
    let jsName = "AtmanDevice"
    let pluginMethods: [CAPPluginMethod] = [
        CAPPluginMethod(name: "scanInvitation", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "loadRecord", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "replaceRecord", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "begin", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "beginRelay", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "read", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "cancel", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "pendingNotification", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "acknowledgeNotification", returnType: CAPPluginReturnPromise),
    ]

    private let owner = DispatchQueue(label: "run.atman.mobile.device")
    private let vault = DeviceVault()
    private var requests: [String: DeviceRequest] = [:]
    @MainActor private var scanner: InvitationScanner?
    @MainActor private var requestingCamera = false

    override func load() {
        NotificationCenter.default.addObserver(self, selector: #selector(notificationOpened),
                                               name: AtmanNoticeInbox.changed, object: nil)
    }

    @objc private func notificationOpened() {
        (bridge as? CapacitorBridge)?.dispatchQueue.async { [weak self] in
            self?.notifyListeners("notificationOpen", data: [:])
        }
    }

    @objc func pendingNotification(_ call: CAPPluginCall) {
        AtmanNoticeInbox.shared.peek { result in
            switch result {
            case .success(let pending): call.resolve(["notice": pending as Any? ?? NSNull()])
            case .failure: call.reject("Could not read the pending notification")
            }
        }
    }

    @objc func acknowledgeNotification(_ call: CAPPluginCall) {
        guard let ticket = call.getString("ticket"), UUID(uuidString: ticket) != nil else {
            call.reject("Invalid notification receipt")
            return
        }
        AtmanNoticeInbox.shared.acknowledge(ticket) { result in
            switch result {
            case .success: call.resolve()
            case .failure: call.reject("Could not acknowledge the notification")
            }
        }
    }

    @objc func scanInvitation(_ call: CAPPluginCall) {
        Task { @MainActor [self] in
            guard scanner == nil, !requestingCamera else {
                call.reject("The scanner is already open.")
                return
            }
            guard DataScannerViewController.isSupported else {
                call.reject("Camera scanning is unavailable. Paste the invitation link.")
                return
            }
            requestingCamera = true
            defer { requestingCamera = false }
            guard await AVCaptureDevice.requestAccess(for: .video) else {
                call.reject("Camera access is off. Enable it in Settings or paste the invitation link.")
                return
            }
            guard DataScannerViewController.isAvailable, let presenter = bridge?.viewController,
                  presenter.viewIfLoaded?.window != nil, presenter.presentedViewController == nil else {
                call.reject("Camera scanning is unavailable. Paste the invitation link.")
                return
            }
            let scanner = InvitationScanner { [weak self] link, unavailable in
                self?.scanner = nil
                if unavailable {
                    call.reject("Camera scanning is unavailable. Paste the invitation link.")
                } else { call.resolve(["link": link as Any? ?? NSNull()]) }
            }
            self.scanner = scanner
            scanner.present(from: presenter)
        }
    }

    @objc func loadRecord(_ call: CAPPluginCall) {
        owner.async {
            do {
                let record = try self.vault.load().flatMap { String(data: $0, encoding: .utf8) }
                call.resolve(["record": record as Any? ?? NSNull()])
            } catch { call.reject("Secure device storage is unavailable") }
        }
    }

    @objc func replaceRecord(_ call: CAPPluginCall) {
        owner.async {
            do {
                if let record = call.getString("record") {
                    let bytes = Data(record.utf8)
                    _ = try self.storedOrigin(self.deviceRecord(bytes))
                    try self.vault.replace(with: bytes)
                } else if call.getValue("record") is NSNull {
                    try self.vault.replace(with: nil)
                } else { throw DeviceBridgeError.invalidRequest }
                call.resolve()
            } catch { call.reject("Could not save the secure device record") }
        }
    }

    @objc func begin(_ call: CAPPluginCall) {
        beginRequest(call, relay: false)
    }

    @objc func beginRelay(_ call: CAPPluginCall) {
        beginRequest(call, relay: true)
    }

    private func beginRequest(_ call: CAPPluginCall, relay: Bool) {
        guard let id = call.getString("id"), UUID(uuidString: id) != nil else {
            call.reject("The device request identity is invalid")
            return
        }
        let retire: () -> Void = { [weak self] in self?.retire(id) }
        owner.async {
            do {
                guard self.requests.count < 12, self.requests[id] == nil else {
                    throw DeviceBridgeError.invalidRequest
                }
                let request = try self.makeRequest(call, relay: relay)
                let reader = DeviceRequest(request, started: call, retire: retire)
                self.requests[id] = reader
                reader.start()
            } catch { call.reject("The device request could not be opened") }
        }
    }

    private func retire(_ id: String) {
        owner.async { self.requests.removeValue(forKey: id) }
    }

    @objc func read(_ call: CAPPluginCall) {
        owner.async {
            guard let id = call.getString("id"), let reader = self.requests[id] else {
                call.reject("The device response has closed")
                return
            }
            reader.read(call)
        }
    }

    @objc func cancel(_ call: CAPPluginCall) {
        owner.async {
            if let id = call.getString("id") { self.requests[id]?.cancel() }
            call.resolve()
        }
    }

    private func makeRequest(_ call: CAPPluginCall, relay: Bool) throws -> URLRequest {
        guard let text = call.getString("url"), text.utf8.count <= 2048,
              let target = URLComponents(string: text), let url = target.url,
              let origin = origin(of: target), let method = call.getString("method") else {
            throw DeviceBridgeError.invalidRequest
        }
        let path = target.percentEncodedPath
        let pairing = !relay && path == "/devices/pair"
        let subscription = path == "/devices/notifications/subscription" || path == "/devices/notifications/native/apns"
        let reads = ["/events", "/devices/me", "/devices/session"]
        let posts = ["/rpc", "/devices/pair", "/devices/refresh"]
        let noticePrefix = "/devices/notifications/"
        let noticeId = String(path.dropFirst(noticePrefix.count))
        let notice = path.hasPrefix(noticePrefix) && UUID(uuidString: noticeId)?.uuidString.lowercased() == noticeId
        let valid = switch method {
        case "GET": relay || subscription || reads.contains(path) || notice
        case "POST": relay || subscription || posts.contains(path)
        default: false
        }
        guard valid, (!relay && path == "/events") || target.percentEncodedQuery == nil else {
            throw DeviceBridgeError.invalidRequest
        }
        var expectedAuthorization: String?
        if !pairing {
            guard let bytes = try vault.load() else { throw DeviceBridgeError.invalidRequest }
            let record = try deviceRecord(bytes)
            if relay {
                expectedAuthorization = try relayAuthorization(record, origin: origin, path: path)
            } else if try storedOrigin(record) != origin {
                throw DeviceBridgeError.invalidRequest
            } else if subscription {
                guard let credentials = record["credentials"] as? [String: Any],
                      let token = credentials["access_token"] as? String else { throw DeviceBridgeError.invalidRequest }
                expectedAuthorization = "Bearer " + token
            }
        }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 40)
        request.httpMethod = method
        request.httpShouldHandleCookies = false
        var authorization = false
        for (field, value) in call.getObject("headers") ?? [:] {
            let name = field.lowercased()
            guard let value = value as? String, value.utf8.count <= 1024,
                  !value.contains("\r"), !value.contains("\n"),
                  ["accept", "authorization", "content-type"].contains(name) else {
                throw DeviceBridgeError.invalidRequest
            }
            if name == "authorization" {
                guard !pairing, !authorization, value.hasPrefix("Bearer "),
                      expectedAuthorization == nil || value == expectedAuthorization else {
                    throw DeviceBridgeError.invalidRequest
                }
                authorization = true
            }
            request.setValue(value, forHTTPHeaderField: name)
        }
        guard pairing || authorization else { throw DeviceBridgeError.invalidRequest }
        if let body = call.getString("body") {
            let limit = relay || subscription ? 4096 : 256 * 1024
            guard method == "POST", body.utf8.count <= limit else {
                throw DeviceBridgeError.invalidRequest
            }
            request.httpBody = Data(body.utf8)
        }
        return request
    }

    private func deviceRecord(_ record: Data) throws -> [String: Any] {
        guard record.count <= 8192, let data = try JSONSerialization.jsonObject(with: record) as? [String: Any] else {
            throw DeviceBridgeError.invalidRequest
        }
        return data
    }

    private func storedOrigin(_ record: [String: Any]) throws -> String {
        guard let credentials = record["credentials"] as? [String: Any],
              let value = credentials["origin"] as? String, let components = URLComponents(string: value),
              components.path.isEmpty || components.path == "/", components.query == nil,
              let origin = origin(of: components) else { throw DeviceBridgeError.invalidRequest }
        return origin
    }

    private func relayAuthorization(_ record: [String: Any], origin: String, path: String) throws -> String {
        guard let binding = record["notifications"] as? [String: Any], binding["kind"] as? String == "relay",
              binding["origin"] as? String == origin, let registration = binding["registration_id"] as? String,
              UUID(uuidString: registration)?.uuidString.lowercased() == registration,
              path == "/v1/registrations/" + registration, let owner = binding["owner_key"] as? String,
              owner.utf8.count == 43, owner.range(of: #"^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$"#,
                                                 options: .regularExpression) != nil else {
            throw DeviceBridgeError.invalidRequest
        }
        return "Bearer " + owner
    }

    private func origin(of url: URLComponents) -> String? {
        guard url.scheme?.lowercased() == "https", let host = url.host, !host.isEmpty,
              url.user == nil, url.password == nil, url.fragment == nil else { return nil }
        var origin = URLComponents()
        origin.scheme = "https"
        origin.host = host.lowercased()
        if url.port != 443 { origin.port = url.port }
        return origin.string
    }
}

@MainActor
private final class InvitationScanner: NSObject, DataScannerViewControllerDelegate {
    private let camera = DataScannerViewController(recognizedDataTypes: [.barcode(symbologies: [.qr])])
    private var completion: ((String?, Bool) -> Void)?

    init(completion: @escaping (String?, Bool) -> Void) {
        self.completion = completion
        super.init()
        camera.delegate = self
        camera.title = "Scan invitation"
        camera.navigationItem.leftBarButtonItem = UIBarButtonItem(
            barButtonSystemItem: .cancel, target: self, action: #selector(cancel))
    }

    func present(from presenter: UIViewController) {
        let sheet = UINavigationController(rootViewController: camera)
        sheet.isModalInPresentation = true
        presenter.present(sheet, animated: true) { [weak self] in
            guard let self, self.completion != nil else { return }
            do { try self.camera.startScanning() }
            catch { self.finish(unavailable: true) }
        }
    }

    func dataScanner(_ dataScanner: DataScannerViewController, didAdd addedItems: [RecognizedItem], allItems: [RecognizedItem]) {
        for item in addedItems {
            if case let .barcode(barcode) = item, let text = barcode.payloadStringValue,
               !text.isEmpty, text.utf8.count <= 2048 {
                finish(link: text)
                return
            }
        }
    }

    func dataScanner(_ dataScanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) {
        finish(unavailable: true)
    }

    @objc private func cancel() { finish() }

    private func finish(link: String? = nil, unavailable: Bool = false) {
        guard let completion else { return }
        self.completion = nil
        camera.stopScanning()
        if let sheet = camera.navigationController, sheet.presentingViewController != nil {
            sheet.dismiss(animated: true) { completion(link, unavailable) }
        } else { completion(link, unavailable) }
    }
}

/// Pull from the original URLSession task without assembling an SSE response in memory.
private final class DeviceRequest: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private let request: URLRequest
    private let retire: () -> Void
    private let queue = DispatchQueue(label: "run.atman.mobile.http")
    private var started: CAPPluginCall?
    private var waiting: CAPPluginCall?
    private var session: URLSession?
    private var task: URLSessionDataTask?
    private var bytes = Data()
    private var suspended = false
    private var ended = false
    private var failure: String?
    private var retired = false

    init(_ request: URLRequest, started: CAPPluginCall, retire: @escaping () -> Void) {
        self.request = request
        self.started = started
        self.retire = retire
    }

    func start() {
        queue.async {
            let delegates = OperationQueue()
            delegates.maxConcurrentOperationCount = 1
            delegates.underlyingQueue = self.queue
            let configuration = URLSessionConfiguration.ephemeral
            configuration.httpCookieStorage = nil
            configuration.urlCredentialStorage = nil
            configuration.timeoutIntervalForResource = 24 * 60 * 60
            let session = URLSession(configuration: configuration, delegate: self, delegateQueue: delegates)
            let task = session.dataTask(with: self.request)
            self.session = session
            self.task = task
            task.resume()
        }
    }

    func read(_ call: CAPPluginCall) {
        queue.async {
            guard self.waiting == nil, !self.retired else {
                call.reject("The device response is unavailable")
                return
            }
            self.waiting = call
            self.deliver()
        }
    }

    func cancel() {
        queue.async {
            self.started?.reject("The device request was cancelled")
            self.waiting?.reject("The device request was cancelled")
            self.started = nil
            self.waiting = nil
            self.close()
        }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        guard let started, let http = response as? HTTPURLResponse, (200...599).contains(http.statusCode) else {
            completionHandler(.cancel)
            return
        }
        self.started = nil
        var reply: PluginCallResultData = ["status": http.statusCode]
        if let type = http.value(forHTTPHeaderField: "Content-Type") { reply["contentType"] = type }
        started.resolve(reply)
        completionHandler(.allow)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard !ended, !retired else { return }
        if data.count > 512 * 1024 - bytes.count {
            failure = "The device response exceeded its buffer"
            bytes.removeAll()
            ended = true
            dataTask.cancel()
        } else {
            bytes.append(data)
            if bytes.count >= 128 * 1024 && !suspended {
                suspended = true
                dataTask.suspend()
            }
        }
        deliver()
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: (any Error)?) {
        guard !retired else { return }
        session.finishTasksAndInvalidate()
        self.session = nil
        self.task = nil
        if error != nil && failure == nil { failure = "The device connection ended unexpectedly" }
        ended = true
        if let started {
            self.started = nil
            started.reject(failure ?? "The host sent no HTTP response")
            close()
        } else { deliver() }
    }

    private func deliver() {
        guard let waiting else { return }
        if !bytes.isEmpty {
            self.waiting = nil
            let count = min(16 * 1024, bytes.count)
            let chunk = bytes.prefix(count).base64EncodedString()
            bytes.removeFirst(count)
            if suspended && bytes.count < 64 * 1024 && !ended {
                suspended = false
                task?.resume()
            }
            waiting.resolve(["bytes": chunk])
        } else if ended {
            self.waiting = nil
            if let failure { waiting.reject(failure) }
            else { waiting.resolve(["bytes": NSNull()]) }
            close()
        }
    }

    private func close() {
        guard !retired else { return }
        retired = true
        bytes.removeAll()
        session?.invalidateAndCancel()
        task = nil
        session = nil
        retire()
    }
}
