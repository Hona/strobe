// Lets bench files import "@hona/strobe-bench" without installing it: imports of the specifier are rewritten
// to the copy strobe ships when the file is loaded.
import { plugin } from "bun"

const index = process.env.STROBE_SDK_INDEX
if (index)
  plugin({
    name: "strobe-sdk",
    setup(build) {
      build.onLoad({ filter: /\.bench\.(ts|tsx|js|mjs)$/ }, async (args) => {
        const source = await Bun.file(args.path).text()
        const contents = source.replace(/(["'])@hona\/strobe-bench\1/g, JSON.stringify(index.replaceAll("\\", "/")))
        return { contents, loader: args.path.endsWith("x") ? "tsx" : "ts" }
      })
    },
  })
