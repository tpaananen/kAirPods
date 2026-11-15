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
        var ring = Battery.splitRing(earbuds(pod(8, true), pod(25, true)))
        compare(ring.left.color, Battery.RED)
        compare(ring.right.color, Battery.ORANGE)
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
}
