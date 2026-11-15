// Run: QT_QPA_PLATFORM=offscreen qmltestrunner -input tests/plasmoid
// Needs the Plasma 6 / Kirigami QML modules installed.
import QtQuick
import QtTest
import "../../plasmoid/contents/ui"

TestCase {
    id: testCase
    name: "FullView"
    when: windowShown
    visible: true
    width: 400
    height: 600

    Component {
        id: fullViewComponent
        FullView {
            width: 300
            height: 500
        }
    }

    function device(address, connected) {
        return { address: address, name: "AirPods Pro 3", connected: connected }
    }

    function createView(devices, selected) {
        var view = createTemporaryObject(fullViewComponent, testCase, {
            devices: devices,
            selectedDevice: selected,
            currentDevice: devices[selected] || null
        })
        verify(view)
        return view
    }

    function findDeviceCard(item) {
        if (item.title === "Device" && item.contentItem !== undefined)
            return item
        for (var i = 0; i < item.children.length; ++i) {
            var found = findDeviceCard(item.children[i])
            if (found)
                return found
        }
        return null
    }

    function deviceCardVisible(view) {
        var card = findDeviceCard(view)
        verify(card, "device card not found")
        return card.visible
    }

    function test_device_card_hidden_when_only_device_disconnected() {
        var view = createView({ "A": device("A", false) }, "A")
        verify(!deviceCardVisible(view))
    }

    function test_device_card_hidden_when_no_listed_device_connected() {
        var view = createView({ "A": device("A", false), "B": device("B", false) }, "A")
        verify(!deviceCardVisible(view))
    }

    function test_device_card_shown_when_only_device_connected() {
        var view = createView({ "A": device("A", true) }, "A")
        verify(deviceCardVisible(view))
    }

    function test_device_card_shown_when_one_of_several_connected() {
        var view = createView({ "A": device("A", true), "B": device("B", false) }, "A")
        verify(deviceCardVisible(view))
    }

    function findDeviceCombo(item) {
        if (item.displayText !== undefined && item.popup !== undefined)
            return item
        for (var i = 0; i < item.children.length; ++i) {
            var found = findDeviceCombo(item.children[i])
            if (found)
                return found
        }
        return null
    }

    function verifyDeviceComboShowsText(view) {
        var combo = findDeviceCombo(view)
        verify(combo, "device selector not found")
        verify(isFinite(combo.leftPadding), "leftPadding is " + combo.leftPadding)
        verify(isFinite(combo.rightPadding), "rightPadding is " + combo.rightPadding)
        verify(isFinite(combo.topPadding), "topPadding is " + combo.topPadding)
        verify(isFinite(combo.bottomPadding), "bottomPadding is " + combo.bottomPadding)
        verify(combo.contentItem.width > 0,
               "selector text area is " + combo.contentItem.width + " px wide")
    }

    function test_device_selector_shows_text_when_shown_from_start() {
        var view = createView({ "A": device("A", true), "B": device("B", false) }, "A")
        waitForRendering(view)
        verifyDeviceComboShowsText(view)
    }

    // The selector is built while the card is hidden (all devices disconnected,
    // e.g. after the adapter was powered off) and shown when one reconnects.
    function test_device_selector_shows_text_after_card_was_hidden() {
        var view = createView({ "A": device("A", false), "B": device("B", false) }, "A")
        waitForRendering(view)
        var connected = { "A": device("A", true), "B": device("B", false) }
        view.devices = connected
        view.currentDevice = connected["A"]
        waitForRendering(view)
        verifyDeviceComboShowsText(view)
    }

    function test_device_card_hides_when_last_device_disconnects() {
        var view = createView({ "A": device("A", true) }, "A")
        verify(deviceCardVisible(view))
        var disconnected = { "A": device("A", false) }
        view.devices = disconnected
        view.currentDevice = disconnected["A"]
        verify(!deviceCardVisible(view))
    }
}
