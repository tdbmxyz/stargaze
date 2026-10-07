//! Known USB controller identities, separate from the generic USB/IP tunnel.
//!
//! IDs are matched individually: Valve also sells non-controller hardware.
//! The new model's IDs are documented in SDL's `controller_list.h`:
//! <https://github.com/libsdl-org/SDL/blob/main/src/joystick/controller_list.h>
//! Bluetooth identities are intentionally absent: forwarding the host's
//! entire Bluetooth adapter would take unrelated devices away too.

const VALVE_VENDOR_ID: u16 = 0x28de;

/// The Steam Deck's built-in controller; forwarding requires explicit opt-in.
pub const BUILTIN_DECK_CONTROLLER: (u16, u16) = (VALVE_VENDOR_ID, 0x1205);

/// A known controller or receiver that can be tunneled as a whole USB device.
pub(super) struct UsbController {
    product_id: u16,
    /// Diagnostic model name (not a replacement USB identity).
    pub(super) model: &'static str,
    built_in: bool,
}

// Add future USB models here only after confirming their VID/PID and purpose.
// Do not add firmware-update/bootloader, Bluetooth, or virtual-gamepad IDs.
const CONTROLLERS: &[UsbController] = &[
    UsbController {
        product_id: 0x1102,
        model: "Steam Controller (original, wired)",
        built_in: false,
    },
    UsbController {
        product_id: 0x1142,
        model: "Steam Controller (original, wireless receiver)",
        built_in: false,
    },
    UsbController {
        product_id: 0x1302,
        model: "Steam Controller (2026, wired)",
        built_in: false,
    },
    UsbController {
        product_id: 0x1304,
        model: "Steam Controller (2026, Proteus puck)",
        built_in: false,
    },
    UsbController {
        product_id: 0x1305,
        model: "Steam Controller (2026, Nereid receiver)",
        built_in: false,
    },
    UsbController {
        product_id: BUILTIN_DECK_CONTROLLER.1,
        model: "Steam Deck built-in controller",
        built_in: true,
    },
];

/// Selects only known USB controller hardware allowed by the handoff policy.
pub(super) fn select(
    vendor: u16,
    product: u16,
    include_builtin: bool,
) -> Option<&'static UsbController> {
    if vendor != VALVE_VENDOR_ID {
        return None;
    }
    CONTROLLERS.iter().find(|controller| {
        controller.product_id == product && (!controller.built_in || include_builtin)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_and_new_usb_controllers_are_selected_by_default() {
        for product in [0x1102, 0x1142, 0x1302, 0x1304, 0x1305] {
            assert!(select(VALVE_VENDOR_ID, product, false).is_some());
            assert!(select(VALVE_VENDOR_ID, product, true).is_some());
        }
    }

    #[test]
    fn deck_requires_explicit_opt_in() {
        let (vendor, product) = BUILTIN_DECK_CONTROLLER;
        assert!(select(vendor, product, false).is_none());
        assert!(select(vendor, product, true).is_some());
    }

    #[test]
    fn unrelated_hardware_and_non_usb_identities_stay_local() {
        for product in [
            0x2432, // Valve WLAN adapter observed on the development host
            0x2101, // VR receiver
            0x11ff, // Steam virtual gamepad
            0x1005, // new controller bootloader
            0x1007, // new receiver bootloader
            0x1105, // original controller Bluetooth
            0x1106, // original controller Bluetooth
            0x1303, // new controller BLE
            0xffff, // unknown future device
        ] {
            for include_builtin in [false, true] {
                assert!(select(VALVE_VENDOR_ID, product, include_builtin).is_none());
            }
        }
        assert!(select(0x045e, 0x1304, true).is_none());
    }

    #[test]
    fn registry_has_unique_product_ids_and_nonempty_model_names() {
        let mut ids = std::collections::HashSet::new();
        for controller in CONTROLLERS {
            assert!(ids.insert(controller.product_id));
            assert_ne!(controller.model, "");
        }
    }
}
