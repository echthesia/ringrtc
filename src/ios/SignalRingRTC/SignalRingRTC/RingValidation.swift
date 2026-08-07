//
// Copyright 2022 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

import SignalRingRTC.RingRTC

/// Type of media for call at time of origination.
public enum CallMediaType: Int32 {
    /// Call should start as audio only.
    case audioCall = 0
    /// Call should start as audio/video.
    case videoCall = 1
}

public func isValidOfferMessage(opaque: Data, messageAgeSec: UInt64, callMediaType: CallMediaType) -> Bool {
    Logger.debug("")

    return opaque.withUnsafeBytes { buffer in
        ringrtcIsValidOffer(AppByteSlice(bytes: buffer.baseAddress?.assumingMemoryBound(to: UInt8.self),
                                         len: buffer.count),
                            messageAgeSec,
                            callMediaType.rawValue)
    }
}

public func isValidOpaqueRing(opaqueCallMessage: Data,
                              messageAgeSec: UInt64,
                              validateGroupRing: (_ groupId: Data, _ ringId: Int64) -> Bool) -> Bool {
    // Use a pointer to the argument to pass a closure down through a C-based interface;
    // withoutActuallyEscaping promises the compiler we won't persist it.
    // This is different from most RingRTC APIs, which are asynchronous; this one is synchronous and stateless.
    withoutActuallyEscaping(validateGroupRing) { validateGroupRing in
        withUnsafePointer(to: validateGroupRing) { validateGroupRingPtr in
            typealias CallbackType = (Data, Int64) -> Bool
            Logger.debug("")

            return opaqueCallMessage.withUnsafeBytes { buffer in
                let opaqueSlice = AppByteSlice(bytes: buffer.baseAddress?.assumingMemoryBound(to: UInt8.self),
                                               len: buffer.count)
                return ringrtcIsCallMessageValidOpaqueRing(opaqueSlice,
                                                           messageAgeSec,
                                                           UnsafeMutableRawPointer(mutating: validateGroupRingPtr)) {
                    (groupId, ringId, context) in
                    let innerValidate = context!.assumingMemoryBound(to: CallbackType.self)
                    return innerValidate.pointee(groupId.asData() ?? Data(), ringId)
                }
            }
        }
    }
}

@available(iOSApplicationExtension, unavailable)
public enum RingUpdate: Int32 {
    /// The sender is trying to ring this user.
    case requested = 0
    /// The sender tried to ring this user, but it's been too long.
    case expiredRing
    /// Call was accepted elsewhere by a different device.
    case acceptedOnAnotherDevice
    /// Call was declined elsewhere by a different device.
    case declinedOnAnotherDevice
    /// This device is currently on a different call.
    case busyLocally
    /// A different device is currently on a different call.
    case busyOnAnotherDevice
    /// The sender cancelled the ring request.
    case cancelledByRinger
}

public func callIdFromEra(_ era: String) -> UInt64 {
    // Necessary because withUTF8 might reallocate to get a contiguous UTF-8 string.
    var era = era
    return era.withUTF8 { eraBytes in
        ringrtcCallIdFromEraId(AppByteSlice(bytes: eraBytes.baseAddress, len: eraBytes.count))
    }
}

public func callIdFromRingId(_ ringId: Int64) -> UInt64 {
    return UInt64(bitPattern: ringId)
}
