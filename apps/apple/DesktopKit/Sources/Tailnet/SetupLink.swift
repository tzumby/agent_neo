import Foundation
import ImageIO
import Vision

/// A setup link for a tester's device: clouddesktop://join?key=tskey-auth-…
/// (scripts/tailnet.py link). Its key is a single-use, pre-approved Tailscale
/// auth key, tagged for the tester: the device joins the desktops tailnet
/// without a Tailscale account, and reaches only that tester's desktop.
public enum SetupLink {
    public static let scheme = "clouddesktop"

    /// The auth key in a link, or in pasted text (the link, or the bare key).
    public static func authKey(in text: String) -> String? {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if let url = URL(string: text), url.scheme != nil { return authKey(in: url) }
        return valid(text)
    }

    public static func authKey(in url: URL) -> String? {
        guard url.scheme?.lowercased() == scheme, url.host()?.lowercased() == "join",
            let key = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "key" })?.value
        else { return nil }
        return valid(key)
    }

    /// Tailscale auth keys: tskey-auth-<id>-<secret>, letters, digits and dashes.
    private static func valid(_ key: String) -> String? {
        guard key.hasPrefix("tskey-auth-"), (20...200).contains(key.count),
            key.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") })
        else { return nil }
        return key
    }
}

// MARK: - Setup codes: the link as a QR code


extension SetupLink {
    /// The key in a picture of a setup code (the QR code scripts/tailnet.py
    /// makes): a screenshot, a photo, an image file.
    public static func authKey(inPicture data: Data) -> String? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
            let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else { return nil }
        return authKey(inPicture: image)
    }

    public static func authKey(inPicture image: CGImage) -> String? {
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        try? VNImageRequestHandler(cgImage: image).perform([request])
        return request.results?.lazy.compactMap(\.payloadStringValue).compactMap { authKey(in: $0) }.first
    }
}
