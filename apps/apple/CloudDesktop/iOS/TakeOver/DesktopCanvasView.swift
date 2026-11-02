import DesktopViewer
import UIKit

/// The desktop's screen, zoomable and pannable, turning touches into pointer
/// events:
/// - tap: click; two-finger tap: right-click;
/// - one-finger drag: move around the zoomed screen; pinch: zoom;
/// - two-finger drag: scroll the page; long-press, then drag: drag on the desktop.
final class DesktopCanvasView: UIView, UIScrollViewDelegate {
    let keyInput: KeyInputView
    private let desktop: RemoteDesktop
    private let scrollView = CanvasScrollView()
    private let screen = UIView()
    private var zoomedToFit = false
    /// The last tapped point (desktop pixels), kept visible when the keyboard opens.
    private var lastTap: CGPoint?
    private var wheelTravel: CGFloat = 0

    init(desktop: RemoteDesktop) {
        self.desktop = desktop
        keyInput = KeyInputView(desktop: desktop)
        super.init(frame: .zero)
        backgroundColor = .black

        scrollView.delegate = self
        scrollView.showsHorizontalScrollIndicator = false
        scrollView.showsVerticalScrollIndicator = false
        scrollView.contentInsetAdjustmentBehavior = .never
        scrollView.decelerationRate = .fast
        // Two fingers scroll the page on the desktop instead of moving the view.
        scrollView.panGestureRecognizer.maximumNumberOfTouches = 1
        addSubview(scrollView)

        screen.layer.magnificationFilter = .linear
        screen.layer.minificationFilter = .trilinear
        scrollView.addSubview(screen)
        addSubview(keyInput)

        let rightClick = UITapGestureRecognizer(target: self, action: #selector(rightClicked))
        rightClick.numberOfTouchesRequired = 2
        let click = UITapGestureRecognizer(target: self, action: #selector(clicked))
        click.require(toFail: rightClick)
        let drag = UILongPressGestureRecognizer(target: self, action: #selector(dragged))
        drag.minimumPressDuration = 0.4
        let wheel = UIPanGestureRecognizer(target: self, action: #selector(scrolled))
        wheel.minimumNumberOfTouches = 2
        wheel.maximumNumberOfTouches = 2
        [rightClick, click, drag, wheel].forEach(screen.addGestureRecognizer)

        desktop.onFrame = { [weak self] image in self?.show(image) }
        if let frame = desktop.frame { show(frame) }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    private func show(_ image: CGImage) {
        let size = CGSize(width: image.width, height: image.height)
        if screen.bounds.size != size {
            scrollView.zoomScale = 1
            screen.frame = CGRect(origin: .zero, size: size)
            scrollView.contentSize = size
            zoomedToFit = false
            setNeedsLayout()
        }
        screen.layer.contents = image
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let previousHeight = scrollView.bounds.height
        scrollView.frame = bounds
        let size = screen.bounds.size
        guard size.width > 0, bounds.width > 0, bounds.height > 0 else { return }

        let fitWidth = bounds.width / size.width
        let fitHeight = bounds.height / size.height
        scrollView.minimumZoomScale = min(fitWidth, fitHeight)
        scrollView.maximumZoomScale = max(3, fitHeight)
        if !zoomedToFit {
            // On a phone in portrait: the full height, centred, which shows the
            // middle of the page (where sign-in forms are) at a readable size.
            zoomedToFit = true
            scrollView.zoomScale = min(max(fitWidth, fitHeight), 1)
            scrollView.contentOffset = CGPoint(
                x: max(0, (scrollView.contentSize.width - bounds.width) / 2), y: 0)
        } else if bounds.height < previousHeight, let lastTap {
            reveal(lastTap)  // the keyboard came up
        }
        centerScreen()
    }

    func viewForZooming(in scrollView: UIScrollView) -> UIView? { screen }

    func scrollViewDidZoom(_ scrollView: UIScrollView) { centerScreen() }

    /// Centres the screen while it's smaller than the view.
    private func centerScreen() {
        let content = scrollView.contentSize
        let insetX = max(0, (scrollView.bounds.width - content.width) / 2)
        let insetY = max(0, (scrollView.bounds.height - content.height) / 2)
        scrollView.contentInset = UIEdgeInsets(top: insetY, left: insetX, bottom: insetY, right: insetX)
    }

    private func reveal(_ point: CGPoint) {
        let zoom = scrollView.zoomScale
        let area = CGRect(x: point.x * zoom - 80, y: point.y * zoom - 80, width: 160, height: 160)
        scrollView.scrollRectToVisible(area, animated: true)
    }

    // MARK: Gestures

    @objc private func clicked(_ gesture: UITapGestureRecognizer) {
        let point = gesture.location(in: screen)
        lastTap = point
        desktop.click(x: Int(point.x), y: Int(point.y))
    }

    @objc private func rightClicked(_ gesture: UITapGestureRecognizer) {
        let point = gesture.location(in: screen)
        desktop.click(x: Int(point.x), y: Int(point.y), right: true)
    }

    @objc private func dragged(_ gesture: UILongPressGestureRecognizer) {
        let point = gesture.location(in: screen)
        let x = Int(point.x)
        let y = Int(point.y)
        switch gesture.state {
        case .began:
            scrollView.remoteDragging = true
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            desktop.move(x: x, y: y)
            desktop.move(x: x, y: y, pressed: true)
        case .changed:
            desktop.move(x: x, y: y, pressed: true)
        default:
            desktop.move(x: x, y: y)
            scrollView.remoteDragging = false
        }
    }

    @objc private func scrolled(_ gesture: UIPanGestureRecognizer) {
        let point = gesture.location(in: screen)
        if gesture.state == .began { wheelTravel = 0 }
        // Fingers moving up scroll the page down, as on the phone itself.
        wheelTravel -= gesture.translation(in: self).y
        gesture.setTranslation(.zero, in: self)
        let step: CGFloat = 24
        let clicks = Int(wheelTravel / step)
        if clicks != 0 {
            wheelTravel -= CGFloat(clicks) * step
            desktop.scroll(x: Int(point.x), y: Int(point.y), clicks: clicks)
        }
    }
}

/// Lets a desktop drag (long-press, then move) keep the finger instead of
/// panning the view.
private final class CanvasScrollView: UIScrollView {
    var remoteDragging = false

    override func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        if gestureRecognizer === panGestureRecognizer, remoteDragging { return false }
        return super.gestureRecognizerShouldBegin(gestureRecognizer)
    }
}
