class GreetingsController < ApplicationController
  def show
    render plain: "hello from autopack\n"
  end
end
