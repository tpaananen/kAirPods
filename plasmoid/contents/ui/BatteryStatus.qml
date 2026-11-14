import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import "battery.js" as Battery

Card {
    id: root

    property var device: null

    // L/R earbud level gap; flagged once it reaches Battery.DRIFT_LEVEL
    readonly property int lrDrift: Battery.drift(device?.battery)
    readonly property bool lrDrifting: lrDrift >= Battery.DRIFT_LEVEL
    readonly property bool leftLagging: lrDrifting && device.battery.left.level < device.battery.right.level
    readonly property bool rightLagging: lrDrifting && device.battery.right.level < device.battery.left.level

    // Estimate shown only when available and no component is charging
    readonly property bool showTtl: {
        const hasEstimate = device?.battery_ttl_estimate != null && device?.battery_ttl_estimate !== undefined
        const headphoneCharging = device?.battery?.headphone?.charging ?? false
        const leftCharging = device?.battery?.left?.charging ?? false
        const rightCharging = device?.battery?.right?.charging ?? false
        return hasEstimate && !(headphoneCharging || leftCharging || rightCharging)
    }

    title: i18n("Battery")
    implicitHeight: Kirigami.Units.gridUnit * 8

    function formatBatteryTime(minutes) {
        if (!minutes || minutes <= 0) return ""

        const hours = Math.floor(minutes / 60)
        const mins = minutes % 60

        if (hours === 0) {
            return i18nc("Estimated battery time left; %1 is minutes", "~%1m left", mins)
        } else if (hours === 1 && mins === 0) {
            return i18nc("Estimated battery time left", "~1h left")
        } else if (mins === 0) {
            return i18nc("Estimated battery time left; %1 is hours", "~%1h left", hours)
        } else {
            return i18nc("Estimated battery time left; %1 is hours, %2 is minutes", "~%1h %2m left", hours, mins)
        }
    }

    contentItem: Component {
        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing

            // Battery indicators row
            RowLayout {
                id: ringRow
                spacing: Kirigami.Units.largeSpacing
                Layout.fillWidth: true

                readonly property real ringSize: Kirigami.Units.gridUnit * 3.5
                // Earbuds: the TTL estimate sits between the L and R rings
                readonly property bool ttlBetweenRings: root.showTtl && leftRing.visible && rightRing.visible

                // Single headphone battery (AirPods Max)
                CircularBatteryIndicator {
                    Layout.fillWidth: true
                    Layout.alignment: Qt.AlignCenter
                    visible: !!device?.battery?.headphone
                    label: i18nc("Short label for the AirPods Max battery", "MX")
                    level: device?.battery?.headphone?.level ?? 0
                    charging: !!device?.battery?.headphone?.charging
                    size: Kirigami.Units.gridUnit * 3.5
                    showEarStatus: true
                    inEar: !(!device?.ear_detection?.left_in_ear && !device?.ear_detection?.right_in_ear)
                }

                // Left AirPod
                CircularBatteryIndicator {
                    id: leftRing
                    Layout.fillWidth: true
                    Layout.alignment: Qt.AlignCenter
                    visible: !!device?.battery?.left && !device?.battery?.headphone
                    label: i18nc("Short label for the left earbud battery", "L")
                    level: device?.battery?.left?.level ?? 0
                    charging: !!device?.battery?.left?.charging
                    size: Kirigami.Units.gridUnit * 3.5
                    showEarStatus: true
                    inEar: !!device?.ear_detection?.left_in_ear
                    highlightLevel: root.leftLagging

                    // Battery TTL estimate, centered in the gap between the L
                    // and R circles. Parented here to stay out of the layout;
                    // coordinates are relative to this ring.
                    Text {
                        visible: ringRow.ttlBetweenRings
                        readonly property real circleRight: (leftRing.width + ringRow.ringSize) / 2
                        readonly property real gap: rightRing.x - leftRing.x
                                                    + (rightRing.width - ringRow.ringSize) / 2 - circleRight
                        width: Math.max(0, gap - Kirigami.Units.smallSpacing * 2)
                        x: circleRight + (gap - width) / 2
                        y: (ringRow.ringSize - height) / 2

                        text: formatBatteryTime(device?.battery_ttl_estimate ?? 0)
                        font.pixelSize: Kirigami.Units.gridUnit * 0.6
                        font.weight: Font.Light
                        color: Kirigami.Theme.textColor
                        opacity: 0.6
                        horizontalAlignment: Text.AlignHCenter
                        elide: Text.ElideRight
                    }
                }

                // Right AirPod
                CircularBatteryIndicator {
                    id: rightRing
                    Layout.fillWidth: true
                    Layout.alignment: Qt.AlignCenter
                    visible: !!device?.battery?.right && !device?.battery?.headphone
                    label: i18nc("Short label for the right earbud battery", "R")
                    level: device?.battery?.right?.level ?? 0
                    charging: !!device?.battery?.right?.charging
                    size: Kirigami.Units.gridUnit * 3.5
                    showEarStatus: true
                    inEar: !!device?.ear_detection?.right_in_ear
                    highlightLevel: root.rightLagging
                }

                // Charging case
                CircularBatteryIndicator {
                    Layout.fillWidth: true
                    Layout.alignment: Qt.AlignCenter
                    visible: !!device?.battery?.case && !device?.battery?.headphone
                    label: i18nc("Short label for the charging case battery", "Case")
                    level: device?.battery?.case?.level ?? 0
                    charging: !!device?.battery?.case?.charging
                    size: Kirigami.Units.gridUnit * 3.5
                }
            }

            // Status lines: TTL estimate (only when it can't sit between the
            // L/R rings, e.g. AirPods Max), then the L/R drift notice
            Column {
                Layout.alignment: Qt.AlignHCenter
                Layout.topMargin: Kirigami.Units.smallSpacing
                visible: (root.showTtl && !ringRow.ttlBetweenRings) || root.lrDrifting

                Text {
                    anchors.horizontalCenter: parent.horizontalCenter
                    visible: root.showTtl && !ringRow.ttlBetweenRings

                    text: formatBatteryTime(device?.battery_ttl_estimate ?? 0)
                    font.pixelSize: Kirigami.Units.gridUnit * 0.6
                    font.weight: Font.Light
                    color: Kirigami.Theme.textColor
                    opacity: visible ? 0.6 : 0

                    // Gentle fade in/out when estimate becomes available or unavailable
                    Behavior on opacity {
                        NumberAnimation {
                            duration: 500
                            easing.type: Easing.InOutQuad
                        }
                    }
                }

                Text {
                    anchors.horizontalCenter: parent.horizontalCenter
                    visible: root.lrDrifting
                    text: i18nc("Left and right earbud battery levels differ; %1 is percentage points",
                                "Uneven drain: L/R differ by %1%", root.lrDrift)
                    font.pixelSize: Kirigami.Units.gridUnit * 0.6
                    font.weight: Font.Medium
                    color: Battery.ORANGE
                }
            }
        }
    }
}
