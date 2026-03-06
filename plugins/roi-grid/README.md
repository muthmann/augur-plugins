# ROI Grid

Partitions the sensor into a grid defined by masked hotpixel positions, then finds the largest rectangular regions that contain no hotpixels. These regions are candidates for the hardware ROI — click "Use as ROI" to apply one directly.

## How It Works

The plugin reads the current pixel mask from `CameraConfig` and computes a grid where each axis boundary corresponds to a masked pixel coordinate. It then searches for the largest rectangles within the grid that span only "free" (unmasked) cells. The top-N largest rectangles are displayed as overlay candidates.

The grid recomputes automatically when the pixel mask changes (e.g., after the Hotpixel Detection plugin updates the DEM mask).

## Configuration

| Setting | Default | Description |
|---|---|---|
| Top N | `3` | Number of largest free rectangles to display |
| Show ROI Grid overlay | `false` | Toggle the grid visualization on the preview canvas |

## Execution Phase

`FrameOnly` — operates on the pixel mask in `CameraConfig`. Does not process pixel data or events.

## Published Data

None. This plugin writes overlays to `AnalysisOutput` and can mutate `CameraConfig` (setting the ROI) via the "Use as ROI" button.

## Dependencies

None (but works best alongside Hotpixel Detection, which populates the pixel mask).
