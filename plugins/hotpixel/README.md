# Hotpixel Detection

Detects pixels that fire at abnormally high rates regardless of scene activity. Identified hotpixels can be pushed into the IMX636 hardware DEM (defective event mask) to suppress them before they enter the event stream.

## How It Works

The plugin maintains an exponential moving average of per-pixel event counts across frames. A pixel is flagged as "hot" when its smoothed count exceeds a configurable multiple of the global mean and a minimum absolute count threshold.

## Configuration

| Setting | Default | Description |
|---|---|---|
| Smoothing depth | `16` | Number of frames for the exponential moving average. Higher values produce more stable detection but react slower to changes. |
| Threshold factor | `10.0` | A pixel is flagged when its count exceeds this multiple of the global mean. Lower values are more aggressive. |
| Min absolute count | `5` | Minimum event count per frame for a pixel to be considered hot. Prevents false positives in low-activity scenes. |

## Execution Phase

`FrameOnly` — operates on decoded preview frames only. Does not require raw events.

## Published Data

None. This plugin writes directly to `AnalysisOutput` (overlays and warnings) and can modify `CameraConfig` through the pixel mask.

## Dependencies

None.
