// Regenerates CloudDesktop/Assets.xcassets/AppIcon.appiconset/AppIcon.png:
//   swiftc -O scripts/make-app-icon.swift -o /tmp/make-app-icon && /tmp/make-app-icon icon.png
//   sips -s format jpeg icon.png --out icon.jpg && sips -s format png icon.jpg --out <AppIcon.png>  # drop alpha
// Draws the app icon: a white monitor outline on black (plain shapes; SF
// Symbols may not be used in app icons). 1024×1024; made opaque afterwards with sips.
import AppKit

let size = 1024
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4,
    hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSColor.black.setFill()
NSRect(x: 0, y: 0, width: size, height: size).fill()

// A soft disc behind the monitor, like the glass circle on the splash.
NSColor(white: 0.13, alpha: 1).setFill()
NSBezierPath(ovalIn: NSRect(x: 132, y: 132, width: 760, height: 760)).fill()

NSColor.white.setStroke()
NSColor.white.setFill()
let screen = NSBezierPath(roundedRect: NSRect(x: 292, y: 412, width: 440, height: 290), xRadius: 34, yRadius: 34)
screen.lineWidth = 34
screen.stroke()
NSBezierPath(roundedRect: NSRect(x: 492, y: 330, width: 40, height: 82), xRadius: 6, yRadius: 6).fill()   // neck
NSBezierPath(roundedRect: NSRect(x: 402, y: 306, width: 220, height: 34), xRadius: 17, yRadius: 17).fill() // base

NSGraphicsContext.restoreGraphicsState()
let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
