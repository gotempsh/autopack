require "pg"

app = lambda do |_env|
  body = "hello from autopack (libpq #{PG.library_version} via pg #{PG::VERSION})\n"
  [200, { "content-type" => "text/plain" }, [body]]
end

run app
