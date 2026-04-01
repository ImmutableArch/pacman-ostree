import gi.repository
gi.require_version('OSTree', '1.0')

from gi.repository import OSTree

def main():
    repo = OSTree.Repo.new()