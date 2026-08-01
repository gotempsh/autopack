app = lambda do |_env|
  [200, { "content-type" => "text/plain" }, ["hello from autopack\n"]]
end

run app
