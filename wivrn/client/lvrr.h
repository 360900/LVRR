/*
 * LVRR build switches for the headset app
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

namespace wivrn::lvrr
{
// WiVRn's own settings, environments, wizard and in-stream window. LVRR shows its settings in
// the WayVR dashboard instead and keeps only a small lobby to find and connect to a PC.
inline constexpr bool stock_ui = false;
} // namespace wivrn::lvrr
