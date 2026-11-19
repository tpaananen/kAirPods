.pragma library

// Battery level colors shared by the popup rings and the taskbar ring.

var CRITICAL_LEVEL = 10 // red at or below this
var LOW_LEVEL = 30      // orange below this

var RED = "#F44336"
var ORANGE = "#FF9800"
var GREEN = "#4CAF50"
var GREY = "#757575"    // earbud out of the ear

var DRIFT_LEVEL = 5     // L/R gap (points) worth flagging
var DIM_OPACITY = 0.45  // taskbar ring half of an earbud out of the ear

/**
 * Absolute L/R earbud level difference, or 0 when drift isn't meaningful:
 * missing levels, single-unit headphones, or either earbud charging.
 */
function drift(battery) {
    if (!battery || battery.headphone) return 0
    var l = battery.left, r = battery.right
    if (!l || !r || l.level == null || r.level == null) return 0
    if (l.charging || r.charging) return 0
    return Math.abs(l.level - r.level)
}

/**
 * Taskbar ring split into a left and a right earbud half, or null for
 * headphones / no battery. Each side is {fill, band, color}: fill and band are
 * 0..1 of the half, and band > fill marks the drift highlight. While drifting
 * the higher earbud fills to the lower one's level and its band covers the
 * difference, so the band shows only on the side with more charge left.
 * Each side also has `charging`; a charging side is green like the popup rings.
 * `dim` marks an earbud out of the ear per `earDetection` (the device's
 * ear_detection, omitted while unknown); a charging earbud is in the case
 * and stays bright.
 */
function splitRing(battery, earDetection) {
    if (!battery || battery.headphone?.level != null) return null
    var left = sideLevel(battery.left), right = sideLevel(battery.right)
    var drifting = drift(battery) >= DRIFT_LEVEL
    var low = Math.min(left, right)
    function side(level, pod, inEar) {
        var charging = !!pod?.charging
        return {
            fill: (drifting ? low : level) / 100,
            band: level / 100,
            color: levelColor(level, charging),
            charging: charging,
            dim: !!earDetection && !inEar && !charging
        }
    }
    return {
        left: side(left, battery.left, earDetection?.left_in_ear),
        right: side(right, battery.right, earDetection?.right_in_ear)
    }
}

/**
 * Taskbar ear-status dot as {color, dim}, or null unless both earbuds report
 * a level. It follows the ring halves: with both earbuds in, full color
 * (orange while the lagging earbud of an L/R drift is worn, otherwise green);
 * with one in, that earbud's color dimmed; with both out, or ear detection
 * unknown, grey.
 */
function earDot(device) {
    var battery = device?.battery
    if (!battery || battery.headphone?.level != null) return null
    if (battery.left?.level == null || battery.right?.level == null) return null
    var leftIn = !!device.ear_detection?.left_in_ear
    var rightIn = !!device.ear_detection?.right_in_ear
    if (!leftIn && !rightIn) return { color: GREY, dim: false }
    var laggingIn = false
    if (drift(battery) >= DRIFT_LEVEL)
        laggingIn = battery.left.level < battery.right.level ? leftIn : rightIn
    return { color: laggingIn ? ORANGE : GREEN, dim: !(leftIn && rightIn) }
}

function sideLevel(pod) {
    if (!pod || pod.level == null) return 0
    return Math.max(0, Math.min(100, pod.level))
}

/** Ring color for a battery `level` (0–100); green while `charging`. */
function levelColor(level, charging) {
    if (charging) return GREEN
    if (level <= CRITICAL_LEVEL) return RED
    if (level < LOW_LEVEL) return ORANGE
    return GREEN
}
