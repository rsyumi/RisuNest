// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import AVFoundation
import UIKit

class CameraView: UIView {
  var videoPreviewLayer: AVCaptureVideoPreviewLayer?

  func interfaceOrientationToVideoOrientation(_ orientation: UIInterfaceOrientation)
    -> AVCaptureVideoOrientation
  {
    switch orientation {
    case UIInterfaceOrientation.portrait:
      return AVCaptureVideoOrientation.portrait
    case UIInterfaceOrientation.portraitUpsideDown:
      return AVCaptureVideoOrientation.portraitUpsideDown
    case UIInterfaceOrientation.landscapeLeft:
      return AVCaptureVideoOrientation.landscapeLeft
    case UIInterfaceOrientation.landscapeRight:
      return AVCaptureVideoOrientation.landscapeRight
    default:
      return AVCaptureVideoOrientation.portraitUpsideDown
    }
  }

  override func layoutSubviews() {
    super.layoutSubviews()
    if let sublayers = self.layer.sublayers {
      for layer in sublayers {
        layer.frame = self.bounds
      }
    }

    if let interfaceOrientation = self.window?
      .windowScene?.interfaceOrientation
    {
      self.videoPreviewLayer?.connection?.videoOrientation = interfaceOrientationToVideoOrientation(
        interfaceOrientation)
    }
  }

  func addPreviewLayer(_ previewLayer: AVCaptureVideoPreviewLayer?) {
    guard let previewLayer = previewLayer else { return }
    previewLayer.videoGravity = AVLayerVideoGravity.resizeAspectFill
    previewLayer.frame = self.bounds
    self.layer.addSublayer(previewLayer)
    self.videoPreviewLayer = previewLayer
  }

  func removePreviewLayer() {
    if self.videoPreviewLayer != nil {
      self.videoPreviewLayer!.removeFromSuperlayer()
      self.videoPreviewLayer = nil
    }
  }
}
