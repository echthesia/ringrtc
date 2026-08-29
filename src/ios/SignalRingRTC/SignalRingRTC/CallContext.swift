//
// Copyright 2019-2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

import SignalRingRTC.RingRTC

#if os(watchOS)

/// One ICE server as `GET /v2/calling/relays` describes it, for the watch's
/// `proceed`: Rust builds the PeerConnection there, so this is the plain data
/// the ObjC SDK's `RTCIceServer` carries on iOS. `hostname` is the TLS host
/// for a `urlsWithIps` entry and empty otherwise.
public struct IceServer: Equatable, Sendable {
    public let urls: [String]
    public let username: String
    public let password: String
    public let hostname: String

    public init(urls: [String], username: String, password: String, hostname: String = "") {
        self.urls = urls
        self.username = username
        self.password = password
        self.hostname = hostname
    }
}

// The watch's call context: the ObjC SDK's sources and tracks live in Rust
// there, and the ICE servers went to Rust with `proceed`, so this is what is
// left for the app to hold per call.
@available(iOSApplicationExtension, unavailable)
public class CallContext {

    let iceServers: [IceServer]
    let hideIp: Bool

    init(iceServers: [IceServer], hideIp: Bool) {
        self.iceServers = iceServers
        self.hideIp = hideIp

        Logger.debug("object! CallContext created... \(ObjectIdentifier(self))")
    }

    deinit {
        Logger.debug("object! CallContext destroyed... \(ObjectIdentifier(self))")
    }

    func getWrapper() -> AppCallContext {
        return AppCallContext(
            object: UnsafeMutableRawPointer(Unmanaged.passRetained(self).toOpaque()),
            destroy: callContextDestroy)
    }
}

#else

import WebRTC

// This class's member functions are all called from the CallManager class
// on the main thread.
@available(iOSApplicationExtension, unavailable)
public class CallContext {

    // A camera queue on which to perform camera operations.
    private static let cameraQueue = DispatchQueue(label: "CallContextCameraQueue")

    let iceServers: [RTCIceServer]
    let hideIp: Bool

    let audioSource: RTCAudioSource
    let audioTrack: RTCAudioTrack
    weak var videoCaptureController: VideoCaptureController!
    let videoSource: RTCVideoSource
    let videoTrack: RTCVideoTrack

    // Cache the latest settings so we don't repeat them.
    var currentVideoEnableSetting: Bool

    init (iceServers: [RTCIceServer], hideIp: Bool, audioSource: RTCAudioSource, audioTrack: RTCAudioTrack, videoSource: RTCVideoSource, videoTrack: RTCVideoTrack, videoCaptureController: VideoCaptureController) {
        self.iceServers = iceServers
        self.hideIp = hideIp
        self.audioSource = audioSource
        self.audioTrack = audioTrack
        self.videoSource = videoSource
        self.videoTrack = videoTrack
        self.videoCaptureController = videoCaptureController

        // For now, assume video starts out as disabled.
        currentVideoEnableSetting = false

        Logger.debug("object! CallContext created... \(ObjectIdentifier(self))")
    }

    deinit {
        Logger.debug("object! CallContext destroyed... \(ObjectIdentifier(self))")
    }

    func getWrapper() -> AppCallContext {
        return AppCallContext(
            object: UnsafeMutableRawPointer(Unmanaged.passRetained(self).toOpaque()),
            destroy: callContextDestroy)
    }

    func getCaptureSession() -> AVCaptureSession {
        return videoCaptureController.captureSession
    }

    func setAudioEnabled(enabled: Bool) {
        audioTrack.isEnabled = enabled
    }

    func setVideoEnabled(enabled: Bool) -> Bool {
        if (enabled == currentVideoEnableSetting) {
            // Video state is not changed.
            return false
        } else {
            videoTrack.isEnabled = enabled
            currentVideoEnableSetting = enabled
            return true
        }
    }

    func setCameraEnabled(enabled: Bool) {
        if enabled {
            videoCaptureController.startCapture()
        } else {
            videoCaptureController.stopCapture()
        }
    }

    func setCameraSource(isUsingFrontCamera: Bool) {
        CallContext.cameraQueue.async {
            self.videoCaptureController.switchCamera(isUsingFrontCamera: isUsingFrontCamera)
        }
    }
}

#endif

@available(iOSApplicationExtension, unavailable)
func callContextDestroy(object: UnsafeMutableRawPointer?) {
    guard let object = object else {
        failDebug("object was unexpectedly nil")
        return
    }

    _ = Unmanaged<CallContext>.fromOpaque(object).takeRetainedValue()
    // @note There should not be any retainers left for the object
    // so deinit should be called implicitly.
}
