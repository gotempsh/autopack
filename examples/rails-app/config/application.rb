require_relative "boot"

require "rails"
require "action_controller/railtie"

module Demo
  class Application < Rails::Application
    config.load_defaults 7.1

    # API-only keeps the example free of an asset pipeline, so the build is
    # about Rails booting rather than about esbuild.
    config.api_only = true
    config.eager_load = true

    # Real apps read this from credentials or the environment; the fallback
    # exists so `docker run` with no configuration still starts.
    config.secret_key_base = ENV.fetch("SECRET_KEY_BASE", "autopack-demo-secret-key-base")

    config.hosts.clear
    config.logger = Logger.new($stdout)
  end
end
