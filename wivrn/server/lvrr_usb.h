/*
 * LVRR: connects headsets plugged in by USB
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

#pragma once

#include <functional>
#include <string>

// Polls adb from the main loop. When a device with the headset app shows up (plugged in, USB
// debugging allowed, or adb back after a drop) while the server waits for a headset, it sets
// up the adb tunnel and starts the app on it, once per appearance.
namespace lvrr_usb
{
struct server
{
	int port;
	// Whether the server waits for a headset
	std::function<bool()> idle;
	// The pairing PIN, empty when pairing is off
	std::function<std::string()> pin;
};

void configure(bool enabled, server);
} // namespace lvrr_usb
