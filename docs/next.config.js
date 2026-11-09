const withNextra = require("nextra")({
  theme: "nextra-theme-docs",
  themeConfig: "./theme.config.tsx",
});

module.exports = {
  output: "export", // For some reason Nextra doesn't pass this though
  images: {
    unoptimized: true,
  },
  ...withNextra(),
};
