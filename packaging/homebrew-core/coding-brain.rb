class CodingBrain < Formula
  desc "Local brain for supervising and learning from coding-agent activity."
  homepage "https://github.com/aleadag/coding-brain"
  url "https://github.com/aleadag/coding-brain/archive/refs/tags/v0.49.3.tar.gz"
  sha256 "601bad4b04822b5910ddd05833de955d238874476270f0a0a26262a8a513b6fd"
  license "MIT"
  head "https://github.com/aleadag/coding-brain.git", branch: "main"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")

    generate_completions_from_executable(bin/"cbrain", "completions")
    (man1/"cbrain.1").write Utils.safe_popen_read(bin/"cbrain", "man")
  end

  test do
    assert_match "cbrain", shell_output("#{bin}/cbrain --version")
    assert_match "cbrain", shell_output("#{bin}/cbrain --help")
    assert_match ".TH cbrain 1", shell_output("#{bin}/cbrain man")
  end
end
