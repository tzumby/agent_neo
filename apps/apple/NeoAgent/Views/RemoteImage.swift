import AgentAPI
import ImageIO
import SwiftUI

#if os(iOS)
    import UIKit
#endif

/// Loads /api/v1/images/<id> through the tailnet session (AsyncImage can't use
/// it). Decoded images are cached, so scrolling back up doesn't refetch them,
/// and decoding happens off the main thread.
struct RemoteImage: View {
    let path: String
    var aspectRatio: CGFloat = 1280.0 / 800.0
    @Environment(\.agentClient) private var client
    @State private var image: Decoded?
    @State private var failed = false

    private static let cache: NSCache<NSString, Decoded> = {
        let cache = NSCache<NSString, Decoded>()
        cache.countLimit = 40
        return cache
    }()

    var body: some View {
        ZStack {
            Rectangle().fill(Theme.inset)
            if let image {
                image.view.resizable().scaledToFill()
            } else if failed {
                Image(systemName: "photo").font(.app(.title2)).foregroundStyle(.tertiary)
            } else {
                ProgressView()
            }
        }
        .aspectRatio(image.map { $0.size.width / max($0.size.height, 1) } ?? aspectRatio, contentMode: .fit)
        .clipShape(.rect(cornerRadius: 16))
        .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(Theme.hairline, lineWidth: 0.5))
        .task(id: path) { await load() }
    }

    private func load() async {
        if let cached = Self.cache.object(forKey: path as NSString) {
            image = cached
            return
        }
        guard let client else { return }
        do {
            let data = try await client.imageData(path: path)
            guard let decoded = await Decoded.from(data) else {
                failed = true
                return
            }
            Self.cache.setObject(decoded, forKey: path as NSString)
            image = decoded
        } catch {
            failed = true
        }
    }
}

/// An image decoded for display, off the main thread.
private final class Decoded: @unchecked Sendable {  // immutable
    #if os(iOS)
        let image: UIImage
        init(_ image: UIImage) { self.image = image }
        var size: CGSize { image.size }
        var view: Image { Image(uiImage: image) }

        static func from(_ data: Data) async -> Decoded? {
            await UIImage(data: data)?.byPreparingForDisplay().map(Decoded.init)
        }
    #else
        let image: CGImage
        init(_ image: CGImage) { self.image = image }
        var size: CGSize { CGSize(width: image.width, height: image.height) }
        var view: Image { Image(decorative: image, scale: 1) }

        static func from(_ data: Data) async -> Decoded? {
            await Task.detached(priority: .userInitiated) {
                guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
                let options = [kCGImageSourceShouldCacheImmediately: true] as CFDictionary
                return CGImageSourceCreateImageAtIndex(source, 0, options).map(Decoded.init)
            }.value
        }
    #endif
}
