// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import AVFoundation
import Tauri
import UIKit
import WebKit

struct ScanOptions: Decodable {
  var formats: [SupportedFormat]?
  var windowed: Bool?
  var cameraDirection: String?
}

enum SupportedFormat: String, CaseIterable, Decodable {
  // UPC_A not supported
  case UPC_E
  case EAN_8
  case EAN_13
  case CODE_39
  case CODE_93
  case CODE_128
  // CODABAR not supported
  case ITF
  case AZTEC
  case DATA_MATRIX
  case PDF_417
  case QR_CODE
  case GS1_DATA_BAR
  case GS1_DATA_BAR_LIMITED
  case GS1_DATA_BAR_EXPANDED

  var value: AVMetadataObject.ObjectType? {
    switch self {
    case .UPC_E: return AVMetadataObject.ObjectType.upce
    case .EAN_8: return AVMetadataObject.ObjectType.ean8
    case .EAN_13: return AVMetadataObject.ObjectType.ean13
    case .CODE_39: return AVMetadataObject.ObjectType.code39
    case .CODE_93: return AVMetadataObject.ObjectType.code93
    case .CODE_128: return AVMetadataObject.ObjectType.code128
    case .ITF: return AVMetadataObject.ObjectType.interleaved2of5
    case .AZTEC: return AVMetadataObject.ObjectType.aztec
    case .DATA_MATRIX: return AVMetadataObject.ObjectType.dataMatrix
    case .PDF_417: return AVMetadataObject.ObjectType.pdf417
    case .QR_CODE: return AVMetadataObject.ObjectType.qr
    case .GS1_DATA_BAR:
      if #available(iOS 15.4, *) {
        return AVMetadataObject.ObjectType.gs1DataBar
      } else {
        return nil
      }
    case .GS1_DATA_BAR_LIMITED:
      if #available(iOS 15.4, *) {
        return AVMetadataObject.ObjectType.gs1DataBarLimited
      } else {
        return nil
      }
    case .GS1_DATA_BAR_EXPANDED:
      if #available(iOS 15.4, *) {
        return AVMetadataObject.ObjectType.gs1DataBarExpanded
      } else {
        return nil
      }
    }
  }
}

enum CaptureError: Error {
  case backCameraUnavailable
  case frontCameraUnavailable
  case couldNotCaptureInput(error: NSError)
}

class BarcodeScannerPlugin: Plugin, AVCaptureMetadataOutputObjectsDelegate {
  var webView: WKWebView!
  var cameraView: CameraView!
  var captureSession: AVCaptureSession?
  var captureVideoPreviewLayer: AVCaptureVideoPreviewLayer?
  var metaOutput: AVCaptureMetadataOutput?

  private let sessionQueue = DispatchQueue(label: "io.github.rsyumi.risunest.scanner")
  private var scanGeneration = 0

  var currentCamera = 0
  var frontCamera: AVCaptureDevice?
  var backCamera: AVCaptureDevice?

  var isScanning = false

  var windowed = false
  var previousBackgroundColor: UIColor? = UIColor.white

  var invoke: Invoke? = nil

  var scanFormats = [AVMetadataObject.ObjectType]()

  public override func load(webview: WKWebView) {
    self.webView = webview
    loadCamera()
  }

  private func loadCamera() {
    cameraView = CameraView(frame: webView.superview?.bounds ?? webView.bounds)
    cameraView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
  }

  public func metadataOutput(
    _ captureOutput: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject],
    from connection: AVCaptureConnection
  ) {
    if metadataObjects.count == 0 || !self.isScanning || captureOutput !== self.metaOutput {
      // while nothing is detected, or if scanning is false, do nothing.
      return
    }

    guard let found = metadataObjects.first as? AVMetadataMachineReadableCodeObject else { return }
    if scanFormats.contains(found.type) {
      var jsObject: JsonObject = [:]

      jsObject["format"] = formatStringFromMetadata(found.type)
      if found.stringValue != nil {
        jsObject["content"] = found.stringValue
      }

      invoke?.resolve(jsObject)
      destroy()

    }
  }

  private func destroy() {
    scanGeneration += 1
    let session = captureSession
    captureSession = nil
    sessionQueue.async { session?.stopRunning() }
    cameraView?.removePreviewLayer()
    cameraView?.removeFromSuperview()
    captureVideoPreviewLayer = nil
    metaOutput = nil
    isScanning = false
    invoke = nil
    if windowed {
      let backgroundColor = previousBackgroundColor ?? UIColor.white
      webView.isOpaque = true
      webView.backgroundColor = backgroundColor
      webView.scrollView.backgroundColor = backgroundColor
    }
    windowed = false
  }

  private func getPermissionState() -> String {
    var permissionState: String

    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
      permissionState = "granted"
    case .denied:
      permissionState = "denied"
    default:
      permissionState = "prompt"
    }

    return permissionState
  }

  @objc override func checkPermissions(_ invoke: Invoke) {
    let permissionState = getPermissionState()
    invoke.resolve(["camera": permissionState])
  }

  @objc override func requestPermissions(_ invoke: Invoke) {
    let state = getPermissionState()
    if state == "prompt" {
      AVCaptureDevice.requestAccess(for: .video) { (authorized) in
        invoke.resolve(["camera": authorized ? "granted" : "denied"])
      }
    } else {
      invoke.resolve(["camera": state])
    }
  }

  @objc func openAppSettings(_ invoke: Invoke) {
    guard let settingsUrl = URL(string: UIApplication.openSettingsURLString) else {
      return
    }

    DispatchQueue.main.async {
      if UIApplication.shared.canOpenURL(settingsUrl) {
        UIApplication.shared.open(
          settingsUrl,
          completionHandler: { (success) in
            invoke.resolve()
          })
      }
    }
  }

  @objc private func scan(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(ScanOptions.self)
    DispatchQueue.main.async {
      guard self.invoke == nil else { invoke.reject("A scan is already running"); return }
      guard let description = Bundle.main.infoDictionary?["NSCameraUsageDescription"] as? String,
            !description.isEmpty else { invoke.reject("NSCameraUsageDescription is not in the app Info.plist"); return }
      guard self.getPermissionState() == "granted" else { invoke.reject("Camera permission denied or not yet requested"); return }
      var formats = [AVMetadataObject.ObjectType]()
      for format in args.formats ?? [] {
        guard let value = format.value else { invoke.reject("Unsupported barcode format on this iOS version"); return }
        formats.append(value)
      }
      let explicitFormats = !formats.isEmpty
      if formats.isEmpty { formats = SupportedFormat.allCases.compactMap { $0.value } }
      self.invoke = invoke
      self.scanGeneration += 1
      let generation = self.scanGeneration
      let requestedFormats = formats
      self.sessionQueue.async {
        do {
          let devices = discoverCaptureDevices()
          let direction: AVCaptureDevice.Position = args.cameraDirection == "front" ? .front : .back
          guard let device = devices.first(where: { $0.position == direction }) ?? devices.first else {
            throw CocoaError(.featureUnsupported)
          }
          let input = try AVCaptureDeviceInput(device: device)
          let session = AVCaptureSession()
          let output = AVCaptureMetadataOutput()
          guard session.canAddInput(input) else { throw CocoaError(.featureUnsupported) }
          session.addInput(input)
          guard session.canAddOutput(output) else { throw CocoaError(.featureUnsupported) }
          session.addOutput(output)
          let supported = requestedFormats.filter { output.availableMetadataObjectTypes.contains($0) }
          guard !supported.isEmpty, !explicitFormats || supported.count == requestedFormats.count else {
            throw CocoaError(.featureUnsupported)
          }
          output.setMetadataObjectsDelegate(self, queue: .main)
          output.metadataObjectTypes = supported
          DispatchQueue.main.async {
            guard self.scanGeneration == generation, self.invoke != nil else { return }
            guard let parent = self.webView.superview else {
              invoke.reject("The app window is unavailable")
              self.destroy()
              return
            }
            self.loadCamera()
            self.cameraView.frame = parent.bounds
            self.cameraView.backgroundColor = .clear
            self.windowed = args.windowed ?? false
            if self.windowed {
              parent.insertSubview(self.cameraView, belowSubview: self.webView)
              self.previousBackgroundColor = self.webView.backgroundColor
              self.webView.isOpaque = false
              self.webView.backgroundColor = .clear
              self.webView.scrollView.backgroundColor = .clear
            } else { parent.insertSubview(self.cameraView, aboveSubview: self.webView) }
            self.captureSession = session
            self.metaOutput = output
            self.scanFormats = supported
            let preview = AVCaptureVideoPreviewLayer(session: session)
            self.captureVideoPreviewLayer = preview
            self.cameraView.addPreviewLayer(preview)
            self.isScanning = true
            self.sessionQueue.async { session.startRunning() }
          }
        } catch {
          DispatchQueue.main.async {
            guard self.scanGeneration == generation else { return }
            invoke.reject("The camera could not be started")
            self.destroy()
          }
        }
      }
    }
  }

  @objc private func cancel(_ invoke: Invoke) {
    DispatchQueue.main.async { [self] in
      self.invoke?.reject("cancelled")
      self.destroy()
      invoke.resolve()
    }
  }
}

@_cdecl("init_plugin_barcode_scanner")
func initPlugin() -> Plugin {
  return BarcodeScannerPlugin()
}
