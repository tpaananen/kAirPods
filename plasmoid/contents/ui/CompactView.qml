import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3
import "battery.js" as Battery

Item {
    id: root

    property var device: null
    property bool showPulseAnimation: true
    property bool showPercentage: false
    property bool horizontalPanel: false
    Layout.minimumWidth: showPercentage && horizontalPanel
                         ? percentageLabel.implicitWidth + Kirigami.Units.smallSpacing + Kirigami.Units.iconSizes.medium
                         : -1
    Layout.preferredWidth: Layout.minimumWidth
    signal clicked()

    property var batteryLevels: {
        if (!device || !device.connected || !device.battery) return []
        const battery = device.battery
        if (battery.headphone?.level != null) return [battery.headphone.level]
        const levels = []
        if (battery.left?.level != null) levels.push(battery.left.level)
        if (battery.right?.level != null) levels.push(battery.right.level)
        return levels
    }
    property int averageBattery: batteryLevels.length
                                 ? Math.round(batteryLevels.reduce((a, b) => a + b, 0) / batteryLevels.length)
                                 : 0
    property int lowestBattery: batteryLevels.length ? Math.min(...batteryLevels) : -1

    // L/R earbud drift: the ring then shows the gap as an orange band
    readonly property int lrDrift: device?.connected ? Battery.drift(device.battery) : 0
    readonly property bool lrDrifting: lrDrift >= Battery.DRIFT_LEVEL

    MouseArea {
        id: mouseArea
        anchors.fill: parent
        onClicked: root.clicked()
        hoverEnabled: true

        // Background with gradient
        Rectangle {
            id: background
            anchors.fill: parent
            radius: width * 0.2

            gradient: Gradient {
                GradientStop { position: 0.0; color: Qt.rgba(255, 255, 255, 0.05) }
                GradientStop { position: 1.0; color: Qt.rgba(255, 255, 255, 0.02) }
            }

            opacity: parent.containsMouse ? 1 : 0
            Behavior on opacity {
                NumberAnimation { duration: 200 }
            }
        }

        Item {
            id: content
            anchors.centerIn: parent
            property real iconSize: Math.round(Math.min(root.height * 0.7, Kirigami.Units.iconSizes.medium))
            property bool showIcon: !root.showPercentage
                                    || root.width >= percentageLabel.implicitWidth + Kirigami.Units.smallSpacing + iconSize
            width: root.showPercentage
                   ? percentageLabel.width + (showIcon ? iconSize + Kirigami.Units.smallSpacing : 0)
                   : Math.round(Math.min(root.width, root.height) * 0.7)
            height: root.height

            // Main glyph: the battery level, or the headphone icon until
            // levels arrive
            Item {
                id: icon
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                // Shrink the glyph (keeping it centered in its slot) so the
                // battery ring or connection pulse has breathing room without
                // growing overall; with neither it fills its slot.
                anchors.leftMargin: (slotWidth - width) / 2
                visible: content.showIcon
                readonly property real slotWidth: root.showPercentage ? content.iconSize : content.width
                readonly property bool ringed: batteryLevels.length > 0 || connectionPulse.visible
                width: ringed ? slotWidth * 0.8 : slotWidth
                height: width

                Kirigami.Icon {
                    objectName: "headphoneIcon"
                    anchors.fill: parent
                    visible: batteryLevels.length === 0
                    source: "audio-headphones"
                }

                // Earbuds show their average; headphones their own level
                PlasmaComponents3.Label {
                    objectName: "centreLevel"
                    anchors.fill: parent
                    visible: batteryLevels.length > 0
                    text: String(averageBattery)
                    horizontalAlignment: Text.AlignHCenter
                    verticalAlignment: Text.AlignVCenter
                    font.pixelSize: parent.height * 0.8
                    font.weight: Font.Medium
                    fontSizeMode: Text.Fit
                    minimumPixelSize: 6
                }

                // Scale animation on hover
                scale: mouseArea.containsMouse ? 1.1 : 1.0
                Behavior on scale {
                    SpringAnimation {
                        spring: 5
                        damping: 0.5
                    }
                }

                // Battery ring: faint track plus an arc showing the charge.
                // Headphones get one clockwise arc from the top. Earbuds get a
                // half per earbud (left half = left earbud), each full at the
                // bottom centre and draining up towards a gap at the top.
                // A charging arc (earbuds in the case) is green with a glow.
                Canvas {
                    id: batteryRing
                    objectName: "batteryRing"
                    visible: batteryLevels.length > 0
                    anchors.centerIn: parent
                    width: Math.round(parent.width * 1.6)
                    height: width
                    rotation: -90 // start from top

                    // Headphones: single arc, so no drift band in practice.
                    readonly property int arcLevel: lrDrifting ? lowestBattery : averageBattery
                    readonly property int bandLevel: lrDrifting ? Math.max(...batteryLevels) : arcLevel
                    property real progress: Math.max(0, Math.min(100, arcLevel)) / 100
                    property real bandEnd: Math.max(0, Math.min(100, bandLevel)) / 100
                    readonly property bool headphoneCharging: !!(device?.connected && device.battery?.headphone?.charging)
                    property color progressColor: Battery.levelColor(arcLevel, headphoneCharging)
                    property color trackColor: Kirigami.ColorUtils.adjustColor(Kirigami.Theme.textColor, {"alpha": -200})

                    // Earbuds: while drifting, the higher earbud's half fills to
                    // the lower one's level and an orange band covers the rest.
                    readonly property var split: device?.connected ? Battery.splitRing(device.battery, device.ear_detection) : null
                    property real leftFill: split ? split.left.fill : 0
                    property real leftBand: split ? split.left.band : 0
                    property real rightFill: split ? split.right.fill : 0
                    property real rightBand: split ? split.right.band : 0
                    property color leftColor: split ? split.left.color : Battery.GREEN
                    property color rightColor: split ? split.right.color : Battery.GREEN
                    property bool leftCharging: split ? split.left.charging : false
                    property bool rightCharging: split ? split.right.charging : false
                    // Out-of-ear earbuds: their half is drawn faded
                    property bool leftDim: split ? split.left.dim : false
                    property bool rightDim: split ? split.right.dim : false
                    // Earbuds: the ear-status dot sits in the gap at the top
                    readonly property bool showEarDot: earDot.visible

                    onProgressChanged: requestPaint()
                    onBandEndChanged: requestPaint()
                    onProgressColorChanged: requestPaint()
                    onTrackColorChanged: requestPaint()
                    onWidthChanged: requestPaint()
                    onSplitChanged: requestPaint()
                    onLeftFillChanged: requestPaint()
                    onLeftBandChanged: requestPaint()
                    onRightFillChanged: requestPaint()
                    onRightBandChanged: requestPaint()
                    onLeftColorChanged: requestPaint()
                    onRightColorChanged: requestPaint()
                    onHeadphoneChargingChanged: requestPaint()
                    onLeftChargingChanged: requestPaint()
                    onRightChargingChanged: requestPaint()
                    onLeftDimChanged: requestPaint()
                    onRightDimChanged: requestPaint()
                    onShowEarDotChanged: requestPaint()

                    // Charging glow, as on the popup rings: the last stroked
                    // arc again, wider and faint. Kept within the canvas edge.
                    function glow(ctx) {
                        ctx.save()
                        ctx.globalAlpha = 0.3
                        ctx.lineWidth = 5
                        ctx.stroke()
                        ctx.restore()
                    }

                    onPaint: {
                        var ctx = getContext("2d")
                        ctx.reset()
                        var c = width / 2
                        var r = c - 2
                        ctx.lineWidth = 3
                        ctx.lineCap = "round"

                        if (split) {
                            paintSplit(ctx, c, r)
                            return
                        }

                        ctx.beginPath()
                        ctx.arc(c, c, r, 0, 2 * Math.PI)
                        ctx.strokeStyle = trackColor
                        ctx.stroke()

                        if (progress > 0) {
                            ctx.beginPath()
                            ctx.arc(c, c, r, 0, 2 * Math.PI * progress)
                            ctx.strokeStyle = progressColor
                            ctx.stroke()
                            if (headphoneCharging)
                                glow(ctx)
                        }

                        if (bandEnd > progress) {
                            ctx.beginPath()
                            ctx.arc(c, c, r, 2 * Math.PI * progress, 2 * Math.PI * bandEnd)
                            ctx.strokeStyle = Battery.ORANGE
                            ctx.stroke()
                        }
                    }

                    // Canvas angle 0 is 12 o'clock (rotated -90); positive runs
                    // clockwise, so the right half spans gap..PI and the left
                    // half mirrors it with negative angles.
                    function paintSplit(ctx, c, r) {
                        // Half-gap at the top: ~3 px stays clear of the round
                        // caps, widened to fit the ear dot when it shows.
                        var gap = (ctx.lineWidth + (showEarDot ? earDot.width / 2 : 0)) / r
                        var span = Math.PI - gap

                        ctx.beginPath()
                        ctx.arc(c, c, r, gap, 2 * Math.PI - gap)
                        ctx.strokeStyle = trackColor
                        ctx.stroke()

                        function half(sign, fill, band, color, charging, dim) {
                            function stroke(from, to, style) {
                                ctx.beginPath()
                                ctx.arc(c, c, r, sign * (gap + span * from), sign * (gap + span * to), sign < 0)
                                ctx.strokeStyle = style
                                ctx.stroke()
                            }
                            // Out of the ear: same length and color, faded.
                            ctx.globalAlpha = dim ? Battery.DIM_OPACITY : 1
                            // Charge runs from the top gap towards the bottom
                            // centre, so draining opens the ring from the bottom.
                            if (fill > 0) {
                                stroke(0, fill, color)
                                if (charging)
                                    glow(ctx)
                            }
                            if (band > fill)
                                stroke(fill, band, Battery.ORANGE)
                            ctx.globalAlpha = 1
                        }
                        half(1, rightFill, rightBand, rightColor, rightCharging, rightDim)
                        half(-1, leftFill, leftBand, leftColor, leftCharging, leftDim)
                    }

                    Behavior on progress {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                    Behavior on bandEnd {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                    Behavior on leftFill {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                    Behavior on leftBand {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                    Behavior on rightFill {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                    Behavior on rightBand {
                        NumberAnimation { duration: 800; easing.type: Easing.InOutCubic }
                    }
                }

                // Ear-status dot at the top of the ring, colored like the
                // popup's ear dots and dimmed like the ring halves while
                // only one earbud is worn (see Battery.earDot)
                Rectangle {
                    id: earDot
                    objectName: "earDot"
                    readonly property var dotState: device?.connected ? Battery.earDot(device) : null
                    visible: dotState !== null
                    width: 6
                    height: width
                    radius: width / 2
                    anchors.horizontalCenter: batteryRing.horizontalCenter
                    y: batteryRing.y + 2 - height / 2 // centred on the ring's stroke
                    color: dotState?.color ?? "transparent"
                    opacity: dotState?.dim ? Battery.DIM_OPACITY : 1
                    Behavior on color {
                        ColorAnimation { duration: 300 }
                    }
                    Behavior on opacity {
                        NumberAnimation { duration: 300 }
                    }
                }

                // Connection pulse
                Rectangle {
                    id: connectionPulse
                    visible: showPulseAnimation && device && device.connected && batteryLevels.length === 0
                    anchors.centerIn: parent
                    width: parent.width + 2 * Math.round(parent.width * 0.15)
                    height: width
                    radius: width / 2
                    color: "transparent"
                    border.width: 2
                    border.color: "#27ae60"
                    opacity: 0

                    SequentialAnimation on opacity {
                        running: parent.visible
                        loops: Animation.Infinite
                        NumberAnimation { to: 0.6; duration: 1500; easing.type: Easing.OutQuad }
                        NumberAnimation { to: 0; duration: 1500; easing.type: Easing.InQuad }
                    }

                    SequentialAnimation on scale {
                        running: parent.visible
                        loops: Animation.Infinite
                        NumberAnimation { from: 0.8; to: 1.2; duration: 3000 }
                    }
                }
            }

            PlasmaComponents3.Label {
                id: percentageLabel
                visible: root.showPercentage
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.leftMargin: content.showIcon ? content.iconSize + Kirigami.Units.smallSpacing : 0
                width: Math.min(implicitWidth, root.width)
                font.pixelSize: Kirigami.Units.gridUnit * 0.7
                fontSizeMode: Text.HorizontalFit
                minimumPixelSize: 6
                text: root.lowestBattery >= 0 ? i18nc("Battery level; %1 is a percentage", "%1%", root.lowestBattery) : "—"
            }
        }
    }
}
