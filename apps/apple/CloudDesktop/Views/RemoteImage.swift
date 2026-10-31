import AgentAPI
import SwiftUI
import UIKit

/// Loads /api/v1/images/<id> through the tailnet session (AsyncImage can't use
/// it). Decoded images are cached, so scrolling back up doesn't refetch them,
/// and decoding happens off the main thread.
struct RemoteImage: View {
    let path: String
    var aspectRatio: CGFloat = 1280.0 / 800.0
    @Environment(\.agentClient) private var client
    @State private var image: UIImage?
    @State private var failed = false

    private static let cache: NSCache<NSString, UIImage> = {
        let cache = NSCache<NSString, UIImage>()
        cache.countLimit = 40
        return cache
    }()

    var body: some View {
        ZStack {
            Rectangle().fill(Theme.inset)
            if let image {
                Image(uiImage: image).resizable().scaledToFill()
            } else if failed {
                Image(systemName: "photo").font(.title2).foregroundStyle(.tertiary)
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
            guard let decoded = await UIImage(data: data)?.byPreparingForDisplay() else {
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
