// A deliberately tiny HTTP server: the point of the example is the build,
// not the networking.
#include <arpa/inet.h>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <netinet/in.h>
#include <string>
#include <unistd.h>

int main() {
  const char *env_port = std::getenv("PORT");
  const int port = env_port ? std::atoi(env_port) : 3000;

  const int server = socket(AF_INET, SOCK_STREAM, 0);
  if (server < 0) {
    std::cerr << "socket failed\n";
    return 1;
  }

  int reuse = 1;
  setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = INADDR_ANY;
  address.sin_port = htons(static_cast<uint16_t>(port));

  if (bind(server, reinterpret_cast<sockaddr *>(&address), sizeof(address)) < 0) {
    std::cerr << "bind failed\n";
    return 1;
  }
  listen(server, 16);
  std::cout << "listening on " << port << std::endl;

  const std::string body = "hello from autopack\n";
  const std::string response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
                               "Content-Length: " + std::to_string(body.size()) +
                               "\r\nConnection: close\r\n\r\n" + body;

  for (;;) {
    const int client = accept(server, nullptr, nullptr);
    if (client < 0) {
      continue;
    }
    char scratch[1024];
    (void)recv(client, scratch, sizeof(scratch), 0);
    (void)send(client, response.c_str(), response.size(), 0);
    close(client);
  }
}
