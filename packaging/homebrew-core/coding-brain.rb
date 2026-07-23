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

    generate_completions_from_executable(bin/"coding-brain", "completions")
    (man1/"coding-brain.1").write Utils.safe_popen_read(bin/"coding-brain", "man")
  end

  test do
    assert_match "coding-brain", shell_output("#{bin}/coding-brain --version")
    assert_match "coding-brain", shell_output("#{bin}/coding-brain --help")
    assert_match ".TH coding-brain 1", shell_output("#{bin}/coding-brain man")
  end
end
