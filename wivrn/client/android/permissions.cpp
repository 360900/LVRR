/*
 * WiVRn VR streaming
 * Copyright (C) 2024  Guillaume Meunier <guillaume.meunier@centraliens.net>
 * Copyright (C) 2024  Patrick Nicolas <patricknicolas@laposte.net>
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

#include "permissions.h"

#include "application.h"
#include "jnipp.h"

#include <deque>
#include <map>
#include <mutex>
#include <set>

static std::mutex permission_callbacks_mutex;
static std::map<int, std::pair<std::string, std::function<void(bool)>>> permission_callbacks;
static int next_permission_request_id;
// LVRR: Android cancels a request made while another one is open, so the others wait for its answer
static bool permission_request_in_flight;
static std::deque<std::pair<std::string, std::function<void(bool)>>> permission_requests;

static bool check_permission(jni::object<jni::details::string_literal<24>{"android/content/Context"}> & ctx, jni::string & permission)
{
	auto result = ctx.call<jni::Int>("checkSelfPermission", permission);
	return result == 0;
}

bool check_permission(const char * permission)
{
	if (not permission)
		return true;

	// Cache positive results to avoid useless jni calls
	static std::set<const char *> permissions;
	static std::mutex mutex;
	std::lock_guard lock(mutex);
	if (permissions.contains(permission))
		return true;

	jni::object<""> act(application::native_app()->activity->clazz);
	auto app = act.call<jni::object<"android/app/Application">>("getApplication");
	auto ctx = app.call<jni::object<"android/content/Context">>("getApplicationContext");

	jni::string jpermission(permission);
	bool res = check_permission(ctx, jpermission);
	if (res)
		permissions.insert(permission);
	return res;
}

void request_permission(const char * permission, std::function<void(bool)> callback)
{
	if (not permission)
	{
		callback(true);
		return;
	}

	jni::object<""> act(application::native_app()->activity->clazz);
	auto app = act.call<jni::object<"android/app/Application">>("getApplication");
	auto ctx = app.call<jni::object<"android/content/Context">>("getApplicationContext");

	jni::string jpermission(permission);
	if (check_permission(ctx, jpermission))
	{
		spdlog::info("{} permission already granted", permission);
		callback(true);
	}
	else
	{
		spdlog::info("{} permission not granted, requesting it", permission);
		jni::array permissions(jpermission);

		int request_code;
		{
			std::lock_guard lock(permission_callbacks_mutex);
			// LVRR: the open request's answer asks for this one
			if (permission_request_in_flight)
			{
				spdlog::info("{} permission request waits for the open one", permission);
				permission_requests.emplace_back(permission, std::move(callback));
				return;
			}
			permission_request_in_flight = true;
			request_code = ++next_permission_request_id;
			permission_callbacks.emplace(request_code, std::make_pair(permission, callback));
		}

		act.call<void>("requestPermissions", permissions, jni::Int(request_code));
	}
}

extern "C" void Java_org_meumeu_wivrn_MainActivity_onRequestPermissionsResult(
        JNIEnv * env,
        jobject instance,
        int requestCode,
        jobjectArray permissions,
        jintArray grantResults);

void Java_org_meumeu_wivrn_MainActivity_onRequestPermissionsResult(
        JNIEnv * env,
        jobject instance,
        int request_code,
        jobjectArray permissions,
        jintArray grant_results)
{
	// LVRR: the waiting requests are asked from this thread, jnipp needs its env
	jni::jni_thread::setup_thread(env);

	std::pair<std::string, std::function<void(bool)>> callback;
	{
		std::lock_guard lock(permission_callbacks_mutex);
		auto it = permission_callbacks.find(request_code);
		if (it == permission_callbacks.end())
		{
			spdlog::info("Ignoring unexpected request code");
			return;
		}

		callback = it->second;
		permission_callbacks.erase(it);
	}

	size_t nb_permissions = std::min(env->GetArrayLength(permissions), env->GetArrayLength(grant_results));

	jint * results = env->GetIntArrayElements(grant_results, nullptr);

	// LVRR: empty arrays mean the prompt was dismissed or cancelled: not granted. The callback
	// runs exactly once, after the arrays are released
	bool granted = false;
	if (nb_permissions == 0)
		spdlog::info("Permission {} request cancelled", callback.first);

	for (size_t i = 0; i < nb_permissions; ++i)
	{
		jstring string = (jstring)(env->GetObjectArrayElement(permissions, i));
		const char * permission = env->GetStringUTFChars(string, nullptr);
		spdlog::info("Permission {} {}", permission, results[i] ? "denied" : "granted");

		if (permission == callback.first)
		{
			granted = results[i] == 0;
		}

		env->ReleaseStringUTFChars(string, permission);
	}

	env->ReleaseIntArrayElements(grant_results, results, 0);

	callback.second(granted);

	// LVRR: requests for the same permission made while its prompt was open share this answer,
	// the others are asked in order, one prompt at a time
	std::deque<std::pair<std::string, std::function<void(bool)>>> waiting;
	{
		std::lock_guard lock(permission_callbacks_mutex);
		permission_request_in_flight = false;
		waiting.swap(permission_requests);
	}
	for (auto & request: waiting)
	{
		if (request.first == callback.first)
			request.second(granted);
		else
			request_permission(request.first.c_str(), std::move(request.second));
	}
}
