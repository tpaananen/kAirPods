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

    function test_device_card_hides_when_last_device_disconnects() {
        var view = createView({ "A": device("A", true) }, "A")
        verify(deviceCardVisible(view))
        var disconnected = { "A": device("A", false) }
        view.devices = disconnected
        view.currentDevice = disconnected["A"]
        verify(!deviceCardVisible(view))
    }
}
