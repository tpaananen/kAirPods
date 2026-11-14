.pragma library

// Battery level colors shared by the popup rings and the taskbar ring.

var CRITICAL_LEVEL = 10 // red at or below this
var LOW_LEVEL = 30      // orange below this

var RED = "#F44336"
var ORANGE = "#FF9800"
var GREEN = "#4CAF50"

var DRIFT_LEVEL = 5     // L/R gap (points) worth flagging

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

/** Ring color for a battery `level` (0–100); green while `charging`. */
function levelColor(level, charging) {
    if (charging) return GREEN
    if (level <= CRITICAL_LEVEL) return RED
    if (level < LOW_LEVEL) return ORANGE
    return GREEN
}
