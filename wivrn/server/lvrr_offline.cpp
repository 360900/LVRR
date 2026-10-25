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

#include "lvrr_offline.h"

#include <fstream>
#include <iostream>

using json = nlohmann::json;

namespace lvrr_offline
{
namespace
{
std::filesystem::path file;
// {"last": key, "headsets": {key: {"settings": HeadsetSettings, "pending": patch}}}
json store = json::object();
std::string current;

// The `config` keys marked "restart" in docs/dbus.md
const char * const restart_keys[] = {
        "resolution_scale",
        "foveation",
        "codec",
        "bit_depth",
        "microphone",
        "mic_unprocessed",
        "hand_tracking",
        "eye_gaze",
        "face_tracking",
        "body_tracking",
};

std::string last()
{
	return store["last"].is_string() ? store["last"].get<std::string>() : "";
}

json * headset(const std::string & key)
{
	if (key.empty() or not store["headsets"].contains(key))
		return nullptr;
	return &store["headsets"][key];
}

void save()
{
	if (file.empty())
		return;
	try
	{
		auto tmp = file;
		tmp += ".tmp";
		std::ofstream(tmp) << store.dump(1, '\t') << '\n';
		std::filesystem::rename(tmp, file);
	}
	catch (std::exception & e)
	{
		std::cerr << "Cannot save " << file << ": " << e.what() << std::endl;
	}
}
} // namespace

void load(const std::filesystem::path & path)
{
	file = path;
	store = json::object();
	current.clear();
	try
	{
		if (std::filesystem::exists(path))
			store = json::parse(std::ifstream(path));
	}
	catch (std::exception & e)
	{
		std::cerr << "Ignoring " << path << ": " << e.what() << std::endl;
	}
	if (not store.is_object())
		store = json::object();
	if (not store["headsets"].is_object())
		store["headsets"] = json::object();
	if (not store["last"].is_string())
		store["last"] = "";
}

void set_headset(const std::string & public_key)
{
	current = public_key;
}

void remember(const json & settings)
{
	if (current.empty())
		return;
	auto & entry = store["headsets"][current];
	entry["settings"] = settings;
	entry["settings"].erase("offline");
	entry["settings"].erase("pending");
	if (not entry.contains("pending"))
		entry["pending"] = nullptr;
	store["last"] = current;
	save();
}

bool known()
{
	auto entry = headset(last());
	return entry and (*entry)["settings"].is_object();
}

json offline_view()
{
	if (not known())
		return nullptr;
	const auto & entry = *headset(last());
	json view = entry["settings"];
	const json & changes = entry["pending"];
	if (changes.is_object() and view["config"].is_object())
		view["config"].update(changes);
	view["offline"] = true;
	view["pending"] = changes.is_object() and not changes.empty() ? changes : json(nullptr);
	// restarting needs a session
	view["restart_required"] = false;
	return view;
}

json stage(const json & patch)
{
	auto entry = headset(last());
	if (not entry)
		return nullptr;
	auto & changes = (*entry)["pending"];
	if (not changes.is_object())
		changes = json::object();
	changes.update(patch);
	save();
	return offline_view();
}

json pending()
{
	auto entry = headset(current);
	if (not entry or not(*entry)["pending"].is_object() or (*entry)["pending"].empty())
		return nullptr;
	return (*entry)["pending"];
}

void clear_pending()
{
	if (auto entry = headset(current))
	{
		(*entry)["pending"] = nullptr;
		save();
	}
}

void forget(const std::string & public_key)
{
	store["headsets"].erase(public_key);
	if (last() == public_key)
		store["last"] = "";
	save();
}

bool needs_restart(const json & patch)
{
	for (const char * key: restart_keys)
		if (patch.contains(key))
			return true;
	return false;
}
} // namespace lvrr_offline
