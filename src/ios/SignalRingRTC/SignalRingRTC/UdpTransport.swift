//
// Copyright 2026 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

// The watch's media transport. The TN3135 call-time networking grant arms
// Network.framework flows only -- BSD sockets fail EHOSTUNREACH for the life
// of a call (measured 2026-08-31: NWConnection STUN round-tripped in 29ms in
// the same instant both BSD probes were refused) -- so WebRTC's packets ride
// NWConnections owned here, driven by RingRTC's injectable network. Rust
// hands outbound datagrams to `sendUdp` on WebRTC's network thread; inbound
// datagrams go back through `ringrtcReceivedUdp`.
//
// One NWConnection per (local, remote) endpoint pair. The virtual local
// endpoint WebRTC bound is requested as the flow's real local endpoint
// (`allowLocalEndpointReuse`), so advertised ports are honest when the bind
// is available; when it is not, the flow falls back to an ephemeral port,
// which keeps STUN/TURN correct (reflexive and relay addresses are per-flow
// facts) and costs only direct host-candidate pairs.

#if os(watchOS)

import Foundation
import Network

final class UdpTransport {
    private let queue = DispatchQueue(label: "org.signal.ringrtc.udp")
    private var connections: [String: NWConnection] = [:]
    /// Keys that already fell back to an ephemeral local port after a bind
    /// failure, so the retry happens once.
    private var fellBack: Set<String> = []

    func getWrapper() -> AppUdpTransport {
        return AppUdpTransport(
            object: UnsafeMutableRawPointer(Unmanaged.passRetained(self).toOpaque()),
            destroy: udpTransportDestroy,
            sendUdp: udpTransportSendUdp)
    }

    fileprivate func send(srcIp: String, srcPort: UInt16, dstIp: String, dstPort: UInt16, data: Data) {
        queue.async {
            self.sendOnQueue(srcIp: srcIp, srcPort: srcPort, dstIp: dstIp, dstPort: dstPort, data: data)
        }
    }

    private func sendOnQueue(srcIp: String, srcPort: UInt16, dstIp: String, dstPort: UInt16, data: Data) {
        let key = "\(srcIp):\(srcPort)->\(dstIp):\(dstPort)"
        let connection = self.connection(key: key, srcIp: srcIp, srcPort: srcPort, dstIp: dstIp, dstPort: dstPort)
        connection?.send(content: data, completion: .contentProcessed { error in
            if let error {
                Logger.warn("UdpTransport send \(key): \(error)")
            }
        })
    }

    private func connection(key: String, srcIp: String, srcPort: UInt16, dstIp: String, dstPort: UInt16) -> NWConnection? {
        if let existing = connections[key] {
            return existing
        }
        guard let remotePort = NWEndpoint.Port(rawValue: dstPort) else {
            Logger.error("UdpTransport: bad remote port \(dstPort)")
            return nil
        }
        let parameters = NWParameters.udp
        parameters.allowLocalEndpointReuse = true
        if !fellBack.contains(key), let localPort = NWEndpoint.Port(rawValue: srcPort) {
            parameters.requiredLocalEndpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(srcIp), port: localPort)
        }
        let connection = NWConnection(host: NWEndpoint.Host(dstIp), port: remotePort, using: parameters)
        connections[key] = connection
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                break
            case .waiting(let error):
                Logger.warn("UdpTransport \(key) waiting: \(error)")
            case .failed(let error):
                Logger.warn("UdpTransport \(key) failed: \(error)")
                self?.queue.async {
                    guard let self else { return }
                    self.connections.removeValue(forKey: key)?.cancel()
                    // One retry without the honest local endpoint: a bind
                    // conflict is the plausible failure, and an ephemeral
                    // port still carries STUN/TURN.
                    if !self.fellBack.contains(key) {
                        self.fellBack.insert(key)
                    }
                }
            case .cancelled:
                self?.queue.async { self?.connections.removeValue(forKey: key) }
            default:
                break
            }
        }
        receiveLoop(connection, key: key, remoteIp: dstIp, remotePort: dstPort, localIp: srcIp, localPort: srcPort)
        connection.start(queue: queue)
        return connection
    }

    private func receiveLoop(_ connection: NWConnection, key: String, remoteIp: String, remotePort: UInt16, localIp: String, localPort: UInt16) {
        connection.receiveMessage { [weak self, weak connection] data, _, _, error in
            if let data, !data.isEmpty {
                Self.inject(data, remoteIp: remoteIp, remotePort: remotePort, localIp: localIp, localPort: localPort)
            }
            if error == nil, let self, let connection {
                self.receiveLoop(connection, key: key, remoteIp: remoteIp, remotePort: remotePort, localIp: localIp, localPort: localPort)
            }
        }
    }

    /// Inbound: (source = the remote we connected to, dest = the virtual
    /// local endpoint WebRTC bound). Rust copies every slice synchronously.
    private static func inject(_ data: Data, remoteIp: String, remotePort: UInt16, localIp: String, localPort: UInt16) {
        let src = Array(remoteIp.utf8)
        let dst = Array(localIp.utf8)
        src.withUnsafeBufferPointer { srcBuffer in
            dst.withUnsafeBufferPointer { dstBuffer in
                data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                    ringrtcReceivedUdp(
                        AppByteSlice(bytes: srcBuffer.baseAddress, len: srcBuffer.count),
                        remotePort,
                        AppByteSlice(bytes: dstBuffer.baseAddress, len: dstBuffer.count),
                        localPort,
                        AppByteSlice(bytes: raw.bindMemory(to: UInt8.self).baseAddress, len: data.count))
                }
            }
        }
    }
}

private func udpTransportDestroy(object: UnsafeMutableRawPointer?) {
    guard let object else { return }
    Unmanaged<UdpTransport>.fromOpaque(object).release()
}

private func udpTransportSendUdp(object: UnsafeMutableRawPointer?, srcIp: AppByteSlice, srcPort: UInt16, dstIp: AppByteSlice, dstPort: UInt16, data: AppByteSlice) {
    guard let object,
          let src = srcIp.asString(),
          let dst = dstIp.asString(),
          let bytes = data.bytes
    else {
        return
    }
    let transport = Unmanaged<UdpTransport>.fromOpaque(object).takeUnretainedValue()
    // Copy before leaving the callback: the slice is borrowed.
    let payload = Data(bytes: bytes, count: data.len)
    transport.send(srcIp: src, srcPort: srcPort, dstIp: dst, dstPort: dstPort, data: payload)
}

#endif
