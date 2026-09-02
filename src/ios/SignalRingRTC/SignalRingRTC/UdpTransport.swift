//
// Copyright 2026 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

// The watch's media transport. The TN3135 call-time networking grant arms
// Network.framework flows only -- BSD sockets fail EHOSTUNREACH for the life
// of a call (measured 2026-08-31: NWConnection STUN round-tripped in 29ms in
// the same instant both BSD probes were refused) -- so WebRTC's packets ride
// Network.framework objects owned here, driven by RingRTC's injectable
// network. Rust hands outbound datagrams to `sendUdp` on WebRTC's network
// thread; inbound datagrams go back through `ringrtcReceivedUdp`.
//
// One NWConnectionGroup per virtual UDP socket WebRTC binds (its local
// endpoint is the group's), sending to any remote and receiving from any
// source, which is what a UDP socket is. It replaced one NWConnection per
// (local, remote) pair: a second connected flow on the same local port sits
// in .waiting(EADDRINUSE) forever, `allowLocalEndpointReuse` notwithstanding
// (measured 2026-08-31, five calls: every ICE check after the first flow's
// starved, so no direct pair ever completed and calls rode the relay). The
// group also takes inbound from an endpoint we never sent to, which
// peer-reflexive candidates need.
//
// The group is a multicast descriptor with unicast traffic enabled --
// Network.framework's spelling of a connectionless socket. The multicast
// member is a formality (the mDNS group the interface already belongs to,
// on our port, which nothing sends to); it is never a destination. No
// entitlement is asked for on watchOS 27 (measured).
//
// The watch has two paths (measured 2026-09-02, shape probes inside a
// granted call, phone nearby): with the phone near, the OS routes flows
// through the iPhone companion tunnel (utun4), where the phone NATs them and
// the group's own sends never complete, but flows extracted from the group
// do; only a flow that REQUIRES the Wi-Fi interface type takes en0 directly,
// where the group sends itself and the router preserves the port. The
// transport follows the OS's choice rather than forcing en0 (the OS picks the
// tunnel for battery, as Apple's own calls do near the phone): every socket
// is an unconstrained group, inbound on its receive handler, outbound on
// extracted per-remote flows. Cost: host candidates are only honest when the
// default path is a direct interface; reflexive and relay candidates are
// learned per socket and work either way.

#if os(watchOS)

import Foundation
import Network

final class UdpTransport {
    private let queue = DispatchQueue(label: "org.signal.ringrtc.udp")
    private var sockets: [LocalEndpoint: Socket] = [:]
    /// Local endpoints whose honest bind was refused, so the ephemeral
    /// fallback happens once per socket rather than on every send.
    private var fellBack: Set<LocalEndpoint> = []

    /// How long a call's sockets outlive `callEnded()`: the engine may still
    /// be flushing (ICE consent, TURN refreshes) as RingRTC reports the end,
    /// and a send after cancellation would only recreate a group for a port
    /// no call will use again.
    private static let retirementGrace: DispatchTimeInterval = .seconds(2)

    private struct LocalEndpoint: Hashable, CustomStringConvertible {
        let ip: String
        let port: UInt16
        var description: String { "\(ip):\(port)" }
    }

    /// One virtual UDP socket. `honestPort` says the group was asked for the
    /// virtual local endpoint itself; false is the ephemeral fallback.
    private final class Socket {
        let local: LocalEndpoint
        let honestPort: Bool
        let group: NWConnectionGroup
        /// Outbound rides per-remote connections extracted from the group
        /// (see the header); the group's own sends only complete on a
        /// Wi-Fi-required group, which this is not.
        var flows: [NWEndpoint: Flow] = [:]
        /// Extraction needs a started group; datagrams sent before `.ready`
        /// wait here (bounded; ICE retransmits).
        var pending: [(NWEndpoint, String, UInt16, Data)] = []
        var isReady = false

        init(local: LocalEndpoint, honestPort: Bool, group: NWConnectionGroup) {
            self.local = local
            self.honestPort = honestPort
            self.group = group
        }

        func cancel() {
            for flow in flows.values { flow.connection.cancel() }
            flows.removeAll()
            group.cancel()
        }
    }

    private final class Flow {
        let connection: NWConnection
        let remoteIp: String
        let remotePort: UInt16
        init(connection: NWConnection, remoteIp: String, remotePort: UInt16) {
            self.connection = connection
            self.remoteIp = remoteIp
            self.remotePort = remotePort
        }
    }

    private static let maxPending = 64

    func getWrapper() -> AppUdpTransport {
        return AppUdpTransport(
            object: UnsafeMutableRawPointer(Unmanaged.passRetained(self).toOpaque()),
            destroy: udpTransportDestroy,
            sendUdp: udpTransportSendUdp)
    }

    /// A call is over: its sockets are dead in WebRTC (the injectable
    /// network never reuses a port), so the groups go too, after a grace
    /// period. Without this a group lingers until its flows error out
    /// (ENOTCONN 47s after one hangup, measured).
    ///
    /// The sockets stay in the map until the grace expires: RingRTC sends
    /// the hangup over RTP data some 30 ms after it reports the end, and a
    /// socket dropped from the map at that moment was recreated for it,
    /// refused its own port (the retiring group still held it) and fell back
    /// to an ephemeral one, so the hangup left from a port the remote had
    /// never seen (measured, two calls).
    func callEnded() {
        queue.async {
            let retiring = self.sockets
            guard !retiring.isEmpty else { return }
            Logger.info("UdpTransport: call ended, retiring \(retiring.count) socket(s)")
            self.queue.asyncAfter(deadline: .now() + Self.retirementGrace) {
                for (local, socket) in retiring {
                    if self.sockets[local] === socket {
                        self.sockets.removeValue(forKey: local)
                    }
                    self.fellBack.remove(local)
                    socket.cancel()
                }
            }
        }
    }

    fileprivate func send(srcIp: String, srcPort: UInt16, dstIp: String, dstPort: UInt16, data: Data) {
        queue.async {
            self.sendOnQueue(local: LocalEndpoint(ip: srcIp, port: srcPort), dstIp: dstIp, dstPort: dstPort, data: data)
        }
    }

    private func sendOnQueue(local: LocalEndpoint, dstIp: String, dstPort: UInt16, data: Data) {
        guard let remotePort = NWEndpoint.Port(rawValue: dstPort) else {
            Logger.error("UdpTransport: bad remote port \(dstPort)")
            return
        }
        guard let socket = socket(for: local) else { return }
        let remote = NWEndpoint.hostPort(host: NWEndpoint.Host(dstIp), port: remotePort)
        guard socket.isReady else {
            if socket.pending.count < Self.maxPending {
                socket.pending.append((remote, dstIp, dstPort, data))
            }
            return
        }
        send(data, from: socket, to: remote, dstIp: dstIp, dstPort: dstPort)
    }

    /// On the queue, group ready.
    private func send(_ data: Data, from socket: Socket, to remote: NWEndpoint, dstIp: String, dstPort: UInt16) {
        let local = socket.local
        guard let flow = flow(from: socket, to: remote, dstIp: dstIp, dstPort: dstPort) else { return }
        flow.connection.send(content: data, completion: .contentProcessed { error in
            if let error {
                Logger.warn("UdpTransport \(local) -> \(dstIp):\(dstPort) flow send: \(error)")
            }
        })
    }

    /// On the queue. One extracted connection per remote endpoint, sharing
    /// the group's local endpoint; nil when the group will not extract.
    private func flow(from socket: Socket, to remote: NWEndpoint, dstIp: String, dstPort: UInt16) -> Flow? {
        if let existing = socket.flows[remote] {
            return existing
        }
        let local = socket.local
        guard let connection = socket.group.extract(connectionTo: remote) else {
            Logger.warn("UdpTransport \(local): group would not extract a connection to \(dstIp):\(dstPort)")
            return nil
        }
        let flow = Flow(connection: connection, remoteIp: dstIp, remotePort: dstPort)
        socket.flows[remote] = flow
        connection.stateUpdateHandler = { [weak self, weak socket, weak flow] state in
            guard let self, let socket, let flow else { return }
            self.queue.async {
                switch state {
                case .waiting(let error):
                    Logger.warn("UdpTransport \(local) -> \(dstIp):\(dstPort) flow waiting: \(error)")
                case .failed(let error):
                    Logger.warn("UdpTransport \(local) -> \(dstIp):\(dstPort) flow failed: \(error)")
                    if socket.flows[remote] === flow { socket.flows.removeValue(forKey: remote) }
                    flow.connection.cancel()
                case .cancelled:
                    if socket.flows[remote] === flow { socket.flows.removeValue(forKey: remote) }
                default:
                    break
                }
            }
        }
        receiveLoop(flow, local: local)
        connection.start(queue: queue)
        return flow
    }

    /// An extracted connection's inbound is its own, not the group's.
    private func receiveLoop(_ flow: Flow, local: LocalEndpoint) {
        flow.connection.receiveMessage { [weak self, weak flow] data, _, _, error in
            guard let flow else { return }
            if let data, !data.isEmpty {
                Self.inject(data, remoteIp: flow.remoteIp, remotePort: flow.remotePort, local: local)
            }
            if error == nil, let self {
                self.receiveLoop(flow, local: local)
            }
        }
    }

    private func socket(for local: LocalEndpoint) -> Socket? {
        if let existing = sockets[local] {
            return existing
        }
        let honestPort = !fellBack.contains(local)
        guard let group = makeGroup(local: local, honestPort: honestPort) else { return nil }
        let socket = Socket(local: local, honestPort: honestPort, group: group)
        sockets[local] = socket
        group.stateUpdateHandler = { [weak self, weak socket] state in
            guard let self, let socket else { return }
            self.queue.async { self.socket(socket, changed: state) }
        }
        group.setReceiveHandler(maximumMessageSize: 65535, rejectOversizedMessages: true) { message, content, _ in
            guard let content, !content.isEmpty,
                  let (remoteIp, remotePort) = Self.ipPort(of: message.remoteEndpoint)
            else { return }
            Self.inject(content, remoteIp: remoteIp, remotePort: remotePort, local: local)
        }
        group.start(queue: queue)
        Logger.info("UdpTransport: socket \(local) \(honestPort ? "on its own port" : "on an ephemeral port")")
        return socket
    }

    private func makeGroup(local: LocalEndpoint, honestPort: Bool) -> NWConnectionGroup? {
        let isV6 = local.ip.contains(":")
        let member = NWEndpoint.hostPort(
            host: isV6 ? "ff02::fb" : "224.0.0.251",
            port: honestPort ? (NWEndpoint.Port(rawValue: local.port) ?? .any) : .any)
        let descriptor: NWMulticastGroup
        do {
            descriptor = try NWMulticastGroup(for: [member], disableUnicast: false)
        } catch {
            Logger.error("UdpTransport: no group descriptor for \(local): \(error)")
            return nil
        }
        let parameters = NWParameters.udp
        parameters.allowLocalEndpointReuse = true
        if honestPort, let port = NWEndpoint.Port(rawValue: local.port) {
            // The wildcard host: an address-bound group extracts nothing on
            // the tunnel, and the OS picks the interface.
            parameters.requiredLocalEndpoint = NWEndpoint.hostPort(host: "0.0.0.0", port: port)
        }
        return NWConnectionGroup(with: descriptor, using: parameters)
    }

    /// On the queue.
    private func socket(_ socket: Socket, changed state: NWConnectionGroup.State) {
        let local = socket.local
        // A retired or replaced socket's news is not ours to act on.
        let current = sockets[local] === socket
        switch state {
        case .setup:
            break
        case .ready:
            Logger.info("UdpTransport \(local) ready")
            if current {
                socket.isReady = true
                let pending = socket.pending
                socket.pending.removeAll()
                for (remote, dstIp, dstPort, data) in pending {
                    send(data, from: socket, to: remote, dstIp: dstIp, dstPort: dstPort)
                }
            }
        case .waiting(let error):
            Logger.warn("UdpTransport \(local) waiting: \(error)")
            // The honest bind was refused. EADDRINUSE is .waiting, not
            // .failed, so the fallback has to be taken from here.
            if current, socket.honestPort, case .posix(let code) = error, code == .EADDRINUSE {
                fallBack(from: socket)
            }
        case .failed(let error):
            Logger.warn("UdpTransport \(local) failed: \(error)")
            if current {
                sockets.removeValue(forKey: local)
                socket.cancel()
                fellBack.insert(local)
            }
        case .cancelled:
            if current {
                sockets.removeValue(forKey: local)
            }
        @unknown default:
            break
        }
    }

    /// On the queue. Replaces a socket whose honest bind was refused with
    /// one on an ephemeral port; the sends queued on the old group are lost,
    /// which ICE and TURN retransmit around.
    private func fallBack(from socket: Socket) {
        let local = socket.local
        fellBack.insert(local)
        sockets.removeValue(forKey: local)
        socket.cancel()
        Logger.warn("UdpTransport \(local): honest port refused, falling back to an ephemeral one")
        _ = self.socket(for: local)
    }

    private static func ipPort(of endpoint: NWEndpoint?) -> (String, UInt16)? {
        guard case .hostPort(let host, let port)? = endpoint else { return nil }
        let ip: String
        switch host {
        case .ipv4(let address):
            ip = "\(address)"
        case .ipv6(let address):
            // Rust parses an IpAddr: no interface scope.
            let text = "\(address)"
            ip = text.split(separator: "%", maxSplits: 1).first.map(String.init) ?? text
        case .name(let name, _):
            ip = name
        @unknown default:
            return nil
        }
        return (ip, port.rawValue)
    }

    /// Inbound: (source = the remote the datagram came from, dest = the
    /// virtual local endpoint WebRTC bound). Rust copies every slice
    /// synchronously.
    private static func inject(_ data: Data, remoteIp: String, remotePort: UInt16, local: LocalEndpoint) {
        let src = Array(remoteIp.utf8)
        let dst = Array(local.ip.utf8)
        src.withUnsafeBufferPointer { srcBuffer in
            dst.withUnsafeBufferPointer { dstBuffer in
                data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                    ringrtcReceivedUdp(
                        AppByteSlice(bytes: srcBuffer.baseAddress, len: srcBuffer.count),
                        remotePort,
                        AppByteSlice(bytes: dstBuffer.baseAddress, len: dstBuffer.count),
                        local.port,
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
