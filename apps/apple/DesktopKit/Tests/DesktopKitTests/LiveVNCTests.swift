import CoreGraphics
import DesktopViewer
import Foundation
import ImageIO
import RFB
import Testing
import UniformTypeIdentifiers

/// Live check against a desktop's x11vnc, e.g. through an SSM port forward:
///   VNC_ADDR=127.0.0.1:15900 swift test --filter LiveVNC
/// Add SOCKS_ADDR, SOCKS_USER and SOCKS_PASS to go through a SOCKS5 proxy (the
/// app's path), and VNC_PNG=<file> to save the first frame. Skipped otherwise.
@Test(.enabled(if: ProcessInfo.processInfo.environment["VNC_ADDR"] != nil))
@MainActor
func liveVNCFirstFrame() async throws {
    let env = ProcessInfo.processInfo.environment
    let (host, port) = try #require(split(env["VNC_ADDR"]))
    let proxy = split(env["SOCKS_ADDR"]).map {
        SOCKSProxy(host: $0.0, port: $0.1, username: env["SOCKS_USER"] ?? "", password: env["SOCKS_PASS"] ?? "")
    }

    let desktop = RemoteDesktop()
    let (frames, sink) = AsyncStream<CGImage>.makeStream()
    desktop.onFrame = { sink.yield($0) }
    let session = Task {
        await desktop.run {
            if let proxy {
                let transport = SOCKSTransport(proxy: proxy, host: host, port: port)
                try await transport.connect()
                return transport
            }
            let transport = RFBTCPTransport(host: host, port: port)
            try await transport.connect()
            return transport
        }
    }
    let timeout = Task {
        try await Task.sleep(for: .seconds(20))
        sink.finish()
    }
    let started = ContinuousClock.now
    var first: CGImage?
    for await frame in frames {
        first = frame
        break
    }
    timeout.cancel()
    session.cancel()

    let image = try #require(first, "no frame within 20 s (state: \(desktop.state))")
    print("first frame \(image.width)x\(image.height) after \(ContinuousClock.now - started)")
    #expect(image.width > 0 && image.height > 0)
    if let path = env["VNC_PNG"] {
        let destination = try #require(
            CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL, UTType.png.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(destination, image, nil)
        #expect(CGImageDestinationFinalize(destination))
    }
}

private func split(_ address: String?) -> (String, UInt16)? {
    guard let address, let colon = address.lastIndex(of: ":"), let port = UInt16(address[address.index(after: colon)...])
    else { return nil }
    return (String(address[..<colon]), port)
}
