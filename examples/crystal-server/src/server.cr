# Crystal ships an HTTP server in its standard library, so this needs no shards.
require "http/server"

port = (ENV["PORT"]? || "3000").to_i

server = HTTP::Server.new do |context|
  context.response.content_type = "text/plain"
  context.response.print "hello from autopack\n"
end

puts "listening on #{port}"
server.listen("0.0.0.0", port)
