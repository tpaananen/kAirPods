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

    // The service reports time left only for parts that drain: none while everything
    // charges, and that of the bud in use while the other charges in the case
    readonly property bool showTtl: (device?.battery_ttl_estimate ?? 0) > 0

    title: i18n("Battery")
    // Time left sits on the title line, for earbuds and AirPods Max alike
    titleDetail: root.showTtl ? formatBatteryTime(device?.battery_ttl_estimate ?? 0) : ""
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

            // L/R drift notice
            Text {
                Layout.alignment: Qt.AlignHCenter
                Layout.topMargin: Kirigami.Units.smallSpacing
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
