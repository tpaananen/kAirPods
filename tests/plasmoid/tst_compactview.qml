// Run: QT_QPA_PLATFORM=offscreen qmltestrunner -input tests/plasmoid
// Needs the Plasma 6 / Kirigami QML modules installed.
import QtQuick
import QtTest
import "../../plasmoid/contents/ui"

// Probes the taskbar battery ring's pixels at clock positions, so the
// on-screen left/right orientation is checked through the canvas rotation.
TestCase {
    id: testCase
    name: "CompactView"
    when: windowShown
    visible: true
    width: 200
    height: 200

    Component {
        id: compactViewComponent
        CompactView {
            width: 120
            height: 120
            showPulseAnimation: false
        }
    }

    function pod(level, charging) {
        return { level: level, charging: !!charging }
    }

    function earbuds(left, right) {
        return { connected: true, battery: { left: left, right: right, case: null, headphone: null } }
    }

    function headphones(level, charging) {
        return { connected: true, battery: { left: null, right: null, case: null, headphone: pod(level, charging) } }
    }

    function findRing(item) {
        if (typeof item.requestPaint === "function")
            return item
        for (var i = 0; i < item.children.length; ++i) {
            var found = findRing(item.children[i])
            if (found)
                return found
        }
        return null
    }

    function createView(device) {
        var view = createTemporaryObject(compactViewComponent, testCase, { device: device })
        verify(view)
        var ring = findRing(view)
        verify(ring, "battery ring not found")
        var centre = ring.parent.mapToItem(view, ring.parent.width / 2, ring.parent.height / 2)
        return { view: view, cx: centre.x, cy: centre.y, r: ring.width / 2 - 2 }
    }

    // Pixel kind at `deg` degrees clockwise from 12 o'clock on screen.
    function kindAt(probe, deg) {
        var a = deg * Math.PI / 180
        var x = Math.round(probe.cx + probe.r * Math.sin(a) - 0.5)
        var y = Math.round(probe.cy - probe.r * Math.cos(a) - 0.5)
        var image = grabImage(probe.view)
        var p = { r: image.red(x, y), g: image.green(x, y), b: image.blue(x, y), a: image.alpha(x, y) }
        function near(c) {
            return p.a > 200 && Math.abs(p.r - c[0]) < 40 && Math.abs(p.g - c[1]) < 40 && Math.abs(p.b - c[2]) < 40
        }
        if (near([0x4C, 0xAF, 0x50])) return "green"
        if (near([0xFF, 0x98, 0x00])) return "orange"
        return "track"
    }

    // Waits out the ring's 800 ms animation, then reports any mismatch.
    function expectKinds(probe, expected) {
        function matches() {
            for (var deg in expected)
                if (kindAt(probe, deg) !== expected[deg])
                    return false
            return true
        }
        for (var waited = 0; waited < 3000 && !matches(); waited += 100)
            wait(100)
        var got = []
        for (var deg in expected)
            got.push(deg + "°=" + kindAt(probe, deg))
        for (deg in expected)
            compare(kindAt(probe, deg), expected[deg], "at " + deg + "°; ring: " + got.join(", "))
    }

    // Left 80, right 40: each half drains from the bottom, and the left half
    // (the higher earbud) shows the 40-point difference in orange.
    function test_drifting_earbuds_split_left_and_right() {
        var probe = createView(earbuds(pod(80), pod(40)))
        expectKinds(probe, { "36": "green", "140": "track", "-36": "green", "-108": "orange", "-165": "track" })
    }

    function test_drift_band_is_only_on_the_higher_earbud() {
        var probe = createView(earbuds(pod(30), pod(90)))
        expectKinds(probe, { "-27": "green", "-100": "track", "27": "green", "100": "orange", "150": "orange" })
    }

    function test_missing_earbud_leaves_its_half_empty() {
        var probe = createView(earbuds(pod(60), null))
        expectKinds(probe, { "-36": "green", "36": "track", "150": "track" })
    }

    function test_full_earbuds_leave_a_gap_at_the_top() {
        var probe = createView(earbuds(pod(100), pod(100)))
        expectKinds(probe, { "0": "track", "90": "green", "-90": "green", "180": "green" })
    }

    // Headphones keep the single clockwise ring from 12 o'clock.
    function test_headphones_keep_single_ring() {
        var probe = createView(headphones(50))
        expectKinds(probe, { "10": "green", "90": "green", "170": "green", "270": "track" })
    }

    // Earbuds in the case at 20%: green while charging, as in the popup,
    // where an idle earbud at that level shows orange.
    function test_charging_earbuds_turn_green() {
        var probe = createView(earbuds(pod(20, true), pod(20)))
        expectKinds(probe, { "-20": "green", "20": "orange" })
    }

    function test_charging_headphones_turn_green() {
        var probe = createView(headphones(20, true))
        expectKinds(probe, { "30": "green" })
    }
}
