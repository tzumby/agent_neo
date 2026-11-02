import AVFoundation
import SwiftUI
import Tailnet
import VisionKit

/// Scans the setup code a tester was sent (a QR code of their setup link,
/// shown on another screen or printed) with the camera. Hands over the key
/// once it sees a setup code; other QR codes are ignored.
struct SetupCodeScanner: View {
    let found: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var camera: Camera = .asking

    enum Camera { case asking, ready, denied, unsupported }

    var body: some View {
        NavigationStack {
            content
                .navigationTitle("Scan setup code")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                }
        }
        .task { camera = await Self.cameraAccess() }
    }

    @ViewBuilder private var content: some View {
        switch camera {
        case .asking:
            ProgressView()
        case .ready:
            ScannerView { key in
                dismiss()
                found(key)
            }
            .ignoresSafeArea()
            .overlay(alignment: .bottom) {
                Text("Point the camera at the setup code you were sent.")
                    .font(.app(.subheadline))
                    .padding(.horizontal, 18)
                    .padding(.vertical, 12)
                    .glass(.capsule)
                    .padding(.bottom, 40)
            }
        case .denied:
            ContentUnavailableView {
                Label("No camera access", systemImage: "camera")
            } description: {
                Text("Allow the camera for Cloud Desktop in Settings, or use a picture of the code instead.")
            } actions: {
                Button("Open Settings") {
                    if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                }
            }
        case .unsupported:
            ContentUnavailableView(
                "Can't scan here", systemImage: "camera",
                description: Text("This device can't scan codes. Use a picture of the code, or paste the link."))
        }
    }

    /// Asks for the camera once; the scanner needs it.
    private static func cameraAccess() async -> Camera {
        guard DataScannerViewController.isSupported else { return .unsupported }
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized: return .ready
        case .notDetermined: return await AVCaptureDevice.requestAccess(for: .video) ? .ready : .denied
        default: return .denied
        }
    }
}

/// VisionKit's live scanner, for QR codes only.
private struct ScannerView: UIViewControllerRepresentable {
    let found: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(found: found) }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])], qualityLevel: .balanced,
            recognizesMultipleItems: false, isHighFrameRateTrackingEnabled: false, isPinchToZoomEnabled: true,
            isGuidanceEnabled: true, isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        return scanner
    }

    // Scanning starts once the view is on screen.
    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        if !scanner.isScanning { try? scanner.startScanning() }
    }

    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning()
    }

    @MainActor final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let found: (String) -> Void
        private var done = false

        init(found: @escaping (String) -> Void) { self.found = found }

        func dataScanner(_ scanner: DataScannerViewController, didAdd added: [RecognizedItem], allItems: [RecognizedItem]) {
            for item in added {
                guard !done, case .barcode(let code) = item, let payload = code.payloadStringValue,
                    let key = SetupLink.authKey(in: payload)
                else { continue }
                done = true
                scanner.stopScanning()
                found(key)
            }
        }
    }
}
