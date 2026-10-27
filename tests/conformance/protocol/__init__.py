"""Client-side codecs for the conformance suite (vendored from the Python server, trimmed), so the suite
talks to a server only over the network and never imports it."""

# Fixed identifiers of Steep's online services (the Python server's constants.py)
APP_ID = "b65780e8-0875-4685-995a-6e952cc605a8"      # Ubi-AppId the game sends
SPACE_ID = "a20f75db-9175-4df6-b1b3-ba42e291166c"    # Steep's UbiServices space
SANDBOX = "STEEP_PC_TU7_A"
CONNECT_APP_ID = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6"       # Ubisoft Connect (PC client)
CONNECT_SPACE_ID = "abda351c-f251-4a70-8fb1-32bd42089262"     # its UbiServices space (login replies)
CREDITS_ITEM = "e3570d44-da4d-40bd-980e-5896bc539d08"         # inventory item holding the credits
