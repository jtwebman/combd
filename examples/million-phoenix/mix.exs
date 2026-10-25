defmodule Million.MixProject do
  use Mix.Project

  def project do
    [
      app: :million,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      releases: [million: [include_executables_for: [:unix]]]
    ]
  end

  def application do
    [extra_applications: [:logger], mod: {Million.Application, []}]
  end

  defp deps do
    [
      {:bandit, "~> 1.13"},
      {:websock_adapter, "~> 0.6"},
      {:exqlite, "~> 0.43"}
    ]
  end
end
