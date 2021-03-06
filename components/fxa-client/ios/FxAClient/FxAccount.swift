/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import Foundation

/// This class inherits from `RustFxAccount` and adds:
/// - Automatic state persistence through `PersistCallback`.
/// - Auth error signaling through observer notifications.
class FxAccount: RustFxAccount {
    private var persistCallback: PersistCallback?

    /// Registers a persistance callback. The callback will get called every time
    /// the `FxAccounts` state needs to be saved. The callback must
    /// persist the passed string in a secure location (like the keychain).
    public func registerPersistCallback(_ cb: PersistCallback) {
        persistCallback = cb
    }

    /// Unregisters a persistance callback.
    public func unregisterPersistCallback() {
        persistCallback = nil
    }

    override func getProfile() throws -> Profile {
        defer { tryPersistState() }
        return try notifyAuthErrors {
            try super.getProfile()
        }
    }

    override func beginOAuthFlow(scopes: [String]) throws -> URL {
        return try notifyAuthErrors {
            try super.beginOAuthFlow(scopes: scopes)
        }
    }

    override func beginPairingFlow(pairingUrl: String, scopes: [String]) throws -> URL {
        return try notifyAuthErrors {
            try super.beginPairingFlow(pairingUrl: pairingUrl, scopes: scopes)
        }
    }

    override func completeOAuthFlow(code: String, state: String) throws {
        defer { tryPersistState() }
        try notifyAuthErrors {
            try super.completeOAuthFlow(code: code, state: state)
        }
    }

    override func getAccessToken(scope: String) throws -> AccessTokenInfo {
        defer { tryPersistState() }
        return try notifyAuthErrors {
            try super.getAccessToken(scope: scope)
        }
    }

    override func disconnect() throws {
        defer { tryPersistState() }
        try super.disconnect()
    }

    override func pollDeviceCommands() throws -> [DeviceEvent] {
        defer { tryPersistState() }
        return try notifyAuthErrors {
            try super.pollDeviceCommands()
        }
    }

    override func fetchDevices() throws -> [Device] {
        return try notifyAuthErrors {
            try super.fetchDevices()
        }
    }

    override func setDevicePushSubscription(endpoint: String, publicKey: String, authKey: String) throws {
        try notifyAuthErrors {
            try super.setDevicePushSubscription(endpoint: endpoint, publicKey: publicKey, authKey: authKey)
        }
    }

    override func setDeviceDisplayName(_ name: String) throws {
        try notifyAuthErrors {
            try super.setDeviceDisplayName(name)
        }
    }

    override func handlePushMessage(payload: String) throws -> [DeviceEvent] {
        defer { tryPersistState() }
        return try notifyAuthErrors {
            try super.handlePushMessage(payload: payload)
        }
    }

    override func sendSingleTab(targetId: String, title: String, url: String) throws {
        return try notifyAuthErrors {
            try super.sendSingleTab(targetId: targetId, title: title, url: url)
        }
    }

    override func initializeDevice(
        name: String,
        deviceType: DeviceType,
        supportedCapabilities: [DeviceCapability]
    ) throws {
        defer { tryPersistState() }
        try notifyAuthErrors {
            try super.initializeDevice(name: name, deviceType: deviceType, supportedCapabilities: supportedCapabilities)
        }
    }

    override func ensureCapabilities(supportedCapabilities: [DeviceCapability]) throws {
        defer { tryPersistState() }
        try notifyAuthErrors {
            try super.ensureCapabilities(supportedCapabilities: supportedCapabilities)
        }
    }

    override func clearAccessTokenCache() throws {
        defer { tryPersistState() }
        try super.clearAccessTokenCache()
    }

    private func tryPersistState() {
        DispatchQueue.global().async {
            guard let cb = self.persistCallback else {
                return
            }
            do {
                let json = try self.toJSON()
                DispatchQueue.global(qos: .background).async {
                    cb.persist(json: json)
                }
            } catch {
                // Ignore the error because the prior operation might have worked,
                // but still log it.
                FxALog.error("FxAccounts internal state serialization failed.")
            }
        }
    }

    internal func notifyAuthErrors<T>(_ cb: () throws -> T) rethrows -> T {
        do {
            return try cb()
        } catch let error as FirefoxAccountError {
            if case let .unauthorized(msg) = error {
                FxALog.debug("Auth error caught: \(msg)")
                notifyAuthError()
            }
            throw error
        }
    }

    internal func notifyAuthError() {
        NotificationCenter.default.post(name: .accountAuthException, object: nil)
    }
}

public protocol PersistCallback {
    func persist(json: String)
}
