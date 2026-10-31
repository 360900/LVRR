/*
 * WiVRn VR streaming
 * Copyright (C) 2022  Guillaume Meunier <guillaume.meunier@centraliens.net>
 * Copyright (C) 2022  Patrick Nicolas <patricknicolas@laposte.net>
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

#include "crypto.h"
#include "wivrn_packets.h"
#include "wivrn_sockets.h"
#include <chrono>
#include <limits>
#include <poll.h>
#include <sys/socket.h>

using namespace wivrn;

class handshake_error : public std::exception
{
	std::string message;

public:
	const char * what() const noexcept override;
	handshake_error(std::string_view message);
};

// LVRR: the server refused this headset (pairing disabled, other version), retrying won't help
class handshake_refused : public handshake_error
{
public:
	using handshake_error::handshake_error;
};

class wivrn_session
{
public:
	using control_socket_t = typed_socket<TCP, to_headset::packets, from_headset::packets>;
	using stream_socket_t = typed_socket<UDP, to_headset::packets, from_headset::packets>;

private:
	control_socket_t control;
	stream_socket_t stream;

	std::atomic<uint64_t> bytes_sent_ = 0;
	std::atomic<uint64_t> bytes_received_ = 0;

	template <typename T>
	void handshake(T address, bool tcp_only, crypto::key & headset_keypair, std::function<std::string(int fd)> pin_enter);

public:
	std::variant<in_addr, in6_addr> address;
	// LVRR: the PC's MAC address, for wake-on-LAN
	std::optional<std::array<uint8_t, 6>> lvrr_mac;
	// LVRR: headset interface the session connected through, empty for a typed address
	std::string lvrr_interface;

	wivrn_session(sockaddr_in6 address, bool tcp_only, crypto::key & headset_keypair, std::function<std::string(int fd)> pin_enter);
	wivrn_session(sockaddr_in address, bool tcp_only, crypto::key & headset_keypair, std::function<std::string(int fd)> pin_enter);
	wivrn_session(const wivrn_session &) = delete;
	wivrn_session & operator=(const wivrn_session &) = delete;

	template <typename T>
	void send_control(T && packet)
	{
		bytes_sent_ += control.send(std::forward<T>(packet));
	}

	template <typename T>
	void send_stream(T && packet)
	{
		if (stream)
			bytes_sent_ += stream.send(std::forward<T>(packet));
		else
			bytes_sent_ += control.send(std::forward<T>(packet));
	}

	template <typename T>
	int poll(T && visitor, std::chrono::milliseconds timeout)
	{
		pollfd fds[2] = {};
		fds[0].events = POLLIN;
		fds[0].fd = stream.get_fd();
		fds[1].events = POLLIN;
		fds[1].fd = control.get_fd();

		while (auto packet = stream.receive_pending(&bytes_received_))
			std::visit(std::forward<T>(visitor), std::move(*packet));
		while (auto packet = control.receive_pending(&bytes_received_))
			std::visit(std::forward<T>(visitor), std::move(*packet));

		int r = ::poll(fds, std::size(fds), timeout.count());
		// LVRR: a signal is not a socket error, the caller polls again
		if (r < 0 and errno == EINTR)
			return 0;
		if (r < 0)
			throw std::system_error(errno, std::system_category());

		if (fds[0].revents & (POLLHUP | POLLERR))
			throw std::runtime_error("Error on stream socket");

		if (fds[1].revents & (POLLHUP | POLLERR))
			throw std::runtime_error("Error on control socket");

		if (fds[0].revents & POLLIN)
		{
			auto packet = stream.receive(&bytes_received_);
			if (packet)
			{
				std::visit(std::forward<T>(visitor), std::move(*packet));
			}
		}

		if (fds[1].revents & POLLIN)
		{
			auto packet = control.receive(&bytes_received_);
			if (packet)
				std::visit(std::forward<T>(visitor), std::move(*packet));
		}

		return r;
	}

	// LVRR: ends the connection but keeps the sockets other threads use open, like the server's
	// wivrn_connection::shutdown(); a thread blocked sending into a full buffer gets an error
	void shutdown()
	{
		if (stream)
			::shutdown(stream.get_fd(), SHUT_RDWR);
		if (control)
		{
			::shutdown(control.get_fd(), SHUT_RDWR);
			// drops the unread input, or a full queue keeps the window shut and the server's send
			// blocked until the socket closes; now its next window probe finds room and its data gets a reset.
			// One call takes it all (MSG_TRUNC needs no buffer), so the network thread's read can't skip bytes
			::recv(control.get_fd(), nullptr, std::numeric_limits<int>::max(), MSG_DONTWAIT | MSG_TRUNC);
		}
	}

	uint64_t bytes_received() const
	{
		return bytes_received_;
	}

	uint64_t bytes_sent() const
	{
		return bytes_sent_;
	}
};
