/*
 * LVRR: headset settings kept while the headset is away
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

#include <filesystem>
#include <nlohmann/json.hpp>
#include <string>

// Each headset's last reported settings and the changes made while it was away, by public
// key, saved to a file. The dashboard shows them as the HeadsetSettings of the last headset
// and the server sends the changes when it connects again (docs/dbus.md).
namespace lvrr_offline
{
// Reads the store; a missing or broken file starts empty, a broken entry is dropped
void load(const std::filesystem::path &);

// The headset of the session that starts or resumes
void set_headset(const std::string & public_key);

// Settings the connected headset reported, as HeadsetSettings JSON
void remember(const nlohmann::json & settings);

// Whether a headset reported settings before
bool known();

// The last headset's settings with its changes applied, marked offline; null when none is known
nlohmann::json offline_view();

// Adds a patch, already validated, to the last headset's changes; returns offline_view()
nlohmann::json stage(const nlohmann::json & patch);

// The connected headset's changes, null when none
nlohmann::json pending();
void clear_pending();

// An unpaired headset is forgotten, also the connected one: its later reports aren't kept
void forget(const std::string & public_key);

// Whether a patch has settings that apply to the next session only
bool needs_restart(const nlohmann::json & patch);
} // namespace lvrr_offline
