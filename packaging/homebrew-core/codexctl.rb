class Codexctl < Formula
  desc "Supervise Codex sessions with a learning local brain"
  homepage "https://github.com/aleadag/codexctl"
  url "https://github.com/aleadag/codexctl/archive/refs/tags/v0.49.3.tar.gz"
  sha256 "601bad4b04822b5910ddd05833de955d238874476270f0a0a26262a8a513b6fd"
  license "MIT"
  head "https://github.com/aleadag/codexctl.git", branch: "main"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")

    generate_completions_from_executable(bin/"codexctl", "completions")

    (man1/"codexctl.1").write Utils.safe_popen_read(bin/"codexctl", "man")
  end

  test do
    # Completions render for the major shells we support
    assert_match "_codexctl", shell_output("#{bin}/codexctl completions bash")
    assert_match "#compdef codexctl", shell_output("#{bin}/codexctl completions zsh")
    assert_match "complete -c codexctl", shell_output("#{bin}/codexctl completions fish")

    # Man page renders to roff
    assert_match ".TH codexctl 1", shell_output("#{bin}/codexctl man")

    # Version surface
    assert_match version.to_s, shell_output("#{bin}/codexctl --version")

    # `--list` against an empty HOME should succeed and produce no sessions.
    # Use a sandboxed HOME so we don't read the user's real ~/.codex.
    ENV["HOME"] = testpath
    system bin/"codexctl", "--list"
  end
end
