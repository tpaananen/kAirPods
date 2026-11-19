// Run: QT_QPA_PLATFORM=offscreen qmltestrunner -input tests/plasmoid
// Needs the Plasma 6 / Kirigami QML modules installed.
import QtQuick
import QtTest
import "../../plasmoid/contents/ui"
import "../../plasmoid/contents/ui/battery.js" as Battery

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

    // Ear detection is left out (unknown, as the service does) unless given.
    function earbuds(left, right, leftIn, rightIn) {
        var device = { connected: true, battery: { left: left, right: right, case: null, headphone: null } }
        if (leftIn !== undefined)
            device.ear_detection = { left_in_ear: !!leftIn, right_in_ear: !!rightIn }
        return device
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
        // Opaque pixel close to a "#RRGGBB" color from battery.js
        function near(hex) {
            var c = [1, 3, 5].map(i => parseInt(hex.substr(i, 2), 16))
            return p.a > 200 && Math.abs(p.r - c[0]) < 40 && Math.abs(p.g - c[1]) < 40 && Math.abs(p.b - c[2]) < 40
        }
        if (near(Battery.GREEN)) return "green"
        if (near(Battery.ORANGE)) return "orange"
        if (near(Battery.GREY)) return "grey"
        // Dimmed (out-of-ear) arcs blend their color into what lies below,
        // so they keep a clear tint where the track is neutral grey.
        if (p.g - p.r > 25 && p.g - p.b > 25) return "dim green"
        if (p.r - p.b > 60 && p.g - p.b > 30) return "dim orange"
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

    // Full earbuds: the halves meet at the bottom; the top gap holds the ear dot.
    function test_full_earbuds_leave_a_gap_at_the_top() {
        var probe = createView(earbuds(pod(100), pod(100)))
        expectKinds(probe, { "0": "grey", "90": "green", "-90": "green", "180": "green" })
    }

    // The dot at 12 o'clock colours like the popup's ear dots.
    function test_ear_dot_is_green_with_both_earbuds_in() {
        var probe = createView(earbuds(pod(70), pod(72), true, true))
        expectKinds(probe, { "0": "green" })
    }

    function test_ear_dot_dims_with_one_earbud_out() {
        var probe = createView(earbuds(pod(70), pod(72), true, false))
        expectKinds(probe, { "0": "dim green" })
    }

    function test_ear_dot_dims_orange_with_only_the_lagging_earbud_in() {
        var probe = createView(earbuds(pod(80), pod(40), false, true))
        expectKinds(probe, { "0": "dim orange" })
    }

    function test_ear_dot_is_grey_with_both_earbuds_out() {
        var probe = createView(earbuds(pod(70), pod(72), false, false))
        expectKinds(probe, { "0": "grey" })
    }

    function test_ear_dot_is_orange_while_the_lagging_earbud_is_in() {
        var probe = createView(earbuds(pod(80), pod(40), true, true))
        expectKinds(probe, { "0": "orange" })
    }

    // An out-of-ear earbud's half dims; length and color still show its level.
    function test_out_of_ear_earbud_half_dims() {
        var probe = createView(earbuds(pod(70), pod(72), true, false))
        expectKinds(probe, { "-90": "green", "90": "dim green" })
    }

    function test_out_of_ear_drift_band_dims_too() {
        var probe = createView(earbuds(pod(80), pod(40), false, true))
        expectKinds(probe, { "-36": "dim green", "-108": "dim orange", "36": "green" })
    }

    function test_charging_earbud_out_of_ear_stays_bright() {
        var probe = createView(earbuds(pod(70, true), pod(72), false, false))
        expectKinds(probe, { "-90": "green", "90": "dim green" })
    }

    function test_earbuds_stay_bright_without_ear_detection() {
        var probe = createView(earbuds(pod(70), pod(72)))
        expectKinds(probe, { "-90": "green", "90": "green" })
    }

    function test_centre_shows_average_earbud_level() {
        var view = createView(earbuds(pod(80), pod(40))).view
        var label = findChild(view, "centreLevel")
        verify(label, "centre level not found")
        verify(label.visible)
        compare(label.text, "60")
        verify(!findChild(view, "headphoneIcon").visible)
    }

    function test_centre_shows_headphone_level() {
        var view = createView(headphones(50)).view
        compare(findChild(view, "centreLevel").text, "50")
    }

    // Nothing connected: the plain headphone icon, with no ring, dot or number.
    function expectIdleIcon(view) {
        verify(findChild(view, "headphoneIcon").visible, "headphone icon hidden")
        verify(!findChild(view, "centreLevel").visible, "centre level shown")
        verify(!findChild(view, "batteryRing").visible, "battery ring shown")
        verify(!findChild(view, "earDot").visible, "ear dot shown")
    }

    function test_no_device_shows_only_the_icon() {
        expectIdleIcon(createView(null).view)
    }

    // Without the ring the icon needs no margin for it: full slot size,
    // where a ringed glyph shrinks to 80 %.
    function test_idle_icon_fills_its_slot() {
        var ringed = findChild(createView(earbuds(pod(80), pod(40))).view, "centreLevel")
        var idle = findChild(createView(null).view, "headphoneIcon")
        fuzzyCompare(idle.width, ringed.width / 0.8, 0.5)
    }

    // A disconnected device may still carry its last battery and ear data.
    function test_disconnected_earbuds_show_only_the_icon() {
        var device = earbuds(pod(80), pod(40), true, true)
        device.connected = false
        expectIdleIcon(createView(device).view)
    }

    function test_disconnected_headphones_show_only_the_icon() {
        var device = headphones(50)
        device.connected = false
        expectIdleIcon(createView(device).view)
    }

    function test_centre_shows_icon_without_levels() {
        var view = createView({ connected: true, battery: null }).view
        verify(!findChild(view, "centreLevel").visible)
        verify(findChild(view, "headphoneIcon").visible)
    }

    // Headphones keep the single clockwise ring from 12 o'clock.
    function test_headphones_keep_single_ring() {
        var probe = createView(headphones(50))
        expectKinds(probe, { "10": "green", "90": "green", "170": "green", "270": "track" })
        verify(!findChild(probe.view, "earDot").visible)
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
