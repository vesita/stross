"""`python -m scripts` 入口：委托给 cli.main()。"""

import sys

from .cli import main

if __name__ == "__main__":
    sys.exit(main())
