defmodule X.MixProject do
  use Mix.Project

  def project do
    [
      app: :x,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      releases: [x: [include_executables_for: [:unix]]]
    ]
  end

  def application do
    [extra_applications: [:logger], mod: {X.Application, []}]
  end

  defp deps do
    [
      {:bandit, "~> 1.14"},
      {:postgrex, "~> 0.21"},
      {:jason, "~> 1.4"}
    ]
  end
end
