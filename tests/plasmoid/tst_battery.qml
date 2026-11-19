// Run: QT_QPA_PLATFORM=offscreen qmltestrunner -input tests/plasmoid
import QtQuick
import QtTest
import "../../plasmoid/contents/ui/battery.js" as Battery

TestCase {
    name: "Battery"

    function pod(level, charging) {
        return { level: level, charging: !!charging }
    }

    function earbuds(left, right) {
        return { left: left, right: right, case: null, headphone: null }
    }

    function budsInEar(left, right, leftIn, rightIn) {
        return { connected: true, battery: earbuds(left, right),
                 ear_detection: { left_in_ear: leftIn, right_in_ear: rightIn } }
    }

    // The top dot follows the ring halves: full color with both earbuds
    // in, dimmed with one out, grey with both out.
    function dot(color, dim) {
        return { color: color, dim: dim }
    }

    function test_split_ring_is_null_for_headphones() {
        compare(Battery.splitRing({ left: null, right: null, case: null, headphone: pod(50) }), null)
    }

    function test_split_ring_is_null_without_battery() {
        compare(Battery.splitRing(null), null)
        compare(Battery.splitRing(undefined), null)
    }

    function test_split_ring_shows_each_earbud_on_its_own_side() {
        var ring = Battery.splitRing(earbuds(pod(72), pod(70)))
        compare(ring.left.fill, 0.72)
        compare(ring.left.band, 0.72)
        compare(ring.right.fill, 0.70)
        compare(ring.right.band, 0.70)
        compare(ring.left.color, Battery.GREEN)
        compare(ring.right.color, Battery.GREEN)
    }

    function test_split_ring_colors_each_side_by_its_own_level() {
        var ring = Battery.splitRing(earbuds(pod(8), pod(25)))
        compare(ring.left.color, Battery.RED)
        compare(ring.right.color, Battery.ORANGE)
    }

    // In the case the earbuds charge: like the popup rings, a charging side
    // turns green whatever its level, and only that side.
    function test_split_ring_is_green_on_the_charging_side() {
        var ring = Battery.splitRing(earbuds(pod(8, true), pod(25)))
        compare(ring.left.color, Battery.GREEN)
        compare(ring.left.charging, true)
        compare(ring.right.color, Battery.ORANGE)
        compare(ring.right.charging, false)
    }

    function test_split_ring_marks_missing_earbud_as_not_charging() {
        var ring = Battery.splitRing(earbuds(pod(10, true), null))
        compare(ring.left.charging, true)
        compare(ring.right.charging, false)
    }

    function test_split_ring_highlights_drift_on_the_higher_earbud() {
        var ring = Battery.splitRing(earbuds(pod(80), pod(40)))
        compare(ring.left.fill, 0.40)
        compare(ring.left.band, 0.80)
        compare(ring.left.color, Battery.GREEN)
        compare(ring.right.fill, 0.40)
        compare(ring.right.band, 0.40)
    }

    function test_split_ring_highlights_drift_when_right_is_higher() {
        var ring = Battery.splitRing(earbuds(pod(30), pod(90)))
        compare(ring.left.fill, 0.30)
        compare(ring.left.band, 0.30)
        compare(ring.right.fill, 0.30)
        compare(ring.right.band, 0.90)
    }

    function test_split_ring_has_no_band_below_drift_threshold() {
        var ring = Battery.splitRing(earbuds(pod(60), pod(60 - Battery.DRIFT_LEVEL + 1)))
        compare(ring.left.band, ring.left.fill)
        compare(ring.right.band, ring.right.fill)
    }

    function test_split_ring_has_no_band_while_an_earbud_charges() {
        var ring = Battery.splitRing(earbuds(pod(90, true), pod(40)))
        compare(ring.left.fill, 0.90)
        compare(ring.left.band, 0.90)
        compare(ring.right.fill, 0.40)
    }

    function test_split_ring_leaves_missing_earbud_empty() {
        var ring = Battery.splitRing(earbuds(pod(60), null))
        compare(ring.left.fill, 0.60)
        compare(ring.right.fill, 0)
        compare(ring.right.band, 0)

        ring = Battery.splitRing(earbuds(pod(null), pod(45)))
        compare(ring.left.fill, 0)
        compare(ring.left.band, 0)
        compare(ring.right.fill, 0.45)
    }

    function test_split_ring_clamps_levels() {
        var ring = Battery.splitRing(earbuds(pod(130, true), pod(-5, true)))
        compare(ring.left.fill, 1)
        compare(ring.right.fill, 0)
    }

    // Out-of-ear earbuds dim their half of the taskbar ring.
    function test_split_ring_dims_an_earbud_out_of_the_ear() {
        var ring = Battery.splitRing(earbuds(pod(70), pod(72)), { left_in_ear: false, right_in_ear: true })
        verify(ring.left.dim)
        verify(!ring.right.dim)
    }

    function test_split_ring_keeps_worn_earbuds_bright() {
        var ring = Battery.splitRing(earbuds(pod(70), pod(72)), { left_in_ear: true, right_in_ear: true })
        verify(!ring.left.dim)
        verify(!ring.right.dim)
    }

    // A charging earbud sits in the case, so it is never worn; keep its
    // charging green at full strength.
    function test_split_ring_keeps_charging_earbuds_bright() {
        var ring = Battery.splitRing(earbuds(pod(70, true), pod(72)), { left_in_ear: false, right_in_ear: false })
        verify(!ring.left.dim)
        verify(ring.right.dim)
    }

    // The service omits ear_detection while it is unknown.
    function test_split_ring_keeps_both_bright_without_ear_detection() {
        var ring = Battery.splitRing(earbuds(pod(70), pod(72)))
        verify(!ring.left.dim)
        verify(!ring.right.dim)
        ring = Battery.splitRing(earbuds(pod(70), pod(72)), null)
        verify(!ring.left.dim)
        verify(!ring.right.dim)
    }

    function test_ear_dot_is_green_with_both_earbuds_in() {
        compare(Battery.earDot(budsInEar(pod(70), pod(72), true, true)), dot(Battery.GREEN, false))
    }

    function test_ear_dot_dims_with_one_earbud_out() {
        compare(Battery.earDot(budsInEar(pod(70), pod(72), true, false)), dot(Battery.GREEN, true))
        compare(Battery.earDot(budsInEar(pod(70), pod(72), false, true)), dot(Battery.GREEN, true))
    }

    function test_ear_dot_is_grey_with_both_earbuds_out() {
        compare(Battery.earDot(budsInEar(pod(70), pod(72), false, false)), dot(Battery.GREY, false))
    }

    function test_ear_dot_is_grey_without_ear_detection() {
        compare(Battery.earDot({ connected: true, battery: earbuds(pod(70), pod(72)) }), dot(Battery.GREY, false))
    }

    // Like the popup's "!" dot: orange while the lagging earbud is worn.
    function test_ear_dot_is_orange_while_the_lagging_earbud_is_in() {
        compare(Battery.earDot(budsInEar(pod(80), pod(40), true, true)), dot(Battery.ORANGE, false))
    }

    function test_ear_dot_dims_orange_with_only_the_lagging_earbud_in() {
        compare(Battery.earDot(budsInEar(pod(80), pod(40), false, true)), dot(Battery.ORANGE, true))
    }

    // Only the higher earbud is worn: nothing worn is lagging, so green.
    function test_ear_dot_dims_green_with_only_the_higher_earbud_in() {
        compare(Battery.earDot(budsInEar(pod(80), pod(40), true, false)), dot(Battery.GREEN, true))
    }

    function test_ear_dot_is_null_for_headphones() {
        var device = { connected: true, battery: { left: null, right: null, case: null, headphone: pod(50) },
                       ear_detection: { left_in_ear: true, right_in_ear: true } }
        compare(Battery.earDot(device), null)
    }

    function test_ear_dot_is_null_without_both_earbuds() {
        compare(Battery.earDot(budsInEar(pod(70), null, true, true)), null)
        compare(Battery.earDot(budsInEar(null, pod(70), true, true)), null)
        compare(Battery.earDot(null), null)
        compare(Battery.earDot({ connected: true, battery: null }), null)
    }
}
