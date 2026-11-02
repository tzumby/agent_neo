import Foundation
import Testing

@testable import Tailnet

@Test func setupLinksCarryOnlyATailscaleAuthKey() throws {
    let key = "tskey-auth-kAbC123CNTRL-0123456789abcdefABCDEF"
    let link = try #require(URL(string: "clouddesktop://join?key=\(key)"))
    #expect(SetupLink.authKey(in: link) == key)
    // Pasted: the link, with spaces around it, or the bare key.
    #expect(SetupLink.authKey(in: "  clouddesktop://join?key=\(key)\n") == key)
    #expect(SetupLink.authKey(in: key) == key)
    // Anything else is refused.
    for text in [
        "https://join?key=\(key)",  // another scheme
        "clouddesktop://login?key=\(key)",  // another action
        "clouddesktop://join?token=\(key)",
        "clouddesktop://join?key=tskey-api-kAbC123CNTRL-0123456789abcdef",  // an API key, not an auth key
        "clouddesktop://join?key=tskey-auth-abc%20def%26x%3D1-0123456789",  // characters a key never has
        "tskey-auth-short",
        "",
    ] {
        #expect(SetupLink.authKey(in: text) == nil, "\(text)")
    }
}

#if os(macOS)
    import CoreImage
    import CoreImage.CIFilterBuiltins

    /// A QR code of `text`, as scripts/tailnet.py makes them, as PNG data.
    private func qrPicture(_ text: String) throws -> Data {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        let image = try #require(filter.outputImage).transformed(by: CGAffineTransform(scaleX: 8, y: 8))
        let context = CIContext()
        return try #require(
            context.pngRepresentation(of: image, format: .RGBA8, colorSpace: CGColorSpaceCreateDeviceRGB()))
    }

    @Test func aPictureOfASetupCodeGivesItsKey() throws {
        let key = "tskey-auth-kAbC123CNTRL-0123456789abcdefABCDEF"
        #expect(SetupLink.authKey(inPicture: try qrPicture("clouddesktop://join?key=\(key)")) == key)
        // A QR code of something else, and something that isn't a picture.
        #expect(SetupLink.authKey(inPicture: try qrPicture("https://example.com")) == nil)
        #expect(SetupLink.authKey(inPicture: Data("not an image".utf8)) == nil)
    }
#endif
